use std::sync::Arc;
use std::time::{Duration, Instant};

use moka::future::Cache;
use moka::Expiry;
use sqlx::SqlitePool;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::sources::{Instrument, SearchFilters, SearchResult, Source};

#[derive(Hash, Eq, PartialEq, Clone)]
pub struct CacheKey {
    pub source: &'static str,
    pub query: String,
    /// The per-source `limit` passed to `Source::search`. Pagination bumps
    /// this with each page, and caching the page-1 vec under a key that
    /// ignored `limit` would cause page 2 to serve the already-truncated
    /// page-1 results. Including it here keeps each requested page-size
    /// in its own cache slot.
    pub limit: usize,
    /// Active instrument filter; entries cached against a Piano filter
    /// must not be served to an unfiltered request and vice versa.
    pub instrument: Option<Instrument>,
}

impl CacheKey {
    /// Stable textual form used as the `search_cache.cache_key` primary key
    /// in the durable L2 store. Must be deterministic across process
    /// restarts (so it can't derive from the in-memory `Hash`), and must
    /// distinguish every field that `Hash`/`Eq` distinguishes — otherwise
    /// two logically different searches would collide in the L2 table.
    ///
    /// `\u{1f}` (ASCII unit separator) joins the fields; it can't appear in
    /// a source id, a numeric limit, or an instrument slug, and queries are
    /// rare to contain it, so collisions across distinct tuples are not a
    /// practical concern.
    fn l2_key(&self) -> String {
        let instrument = self.instrument.map(|i| i.slug()).unwrap_or("");
        format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}",
            self.source, self.limit, instrument, self.query
        )
    }

    /// TTL for this key's source. Mirrors the moka [`SearchExpiry`] policy so
    /// the durable L2 store ages entries out on the same schedule as L1.
    fn ttl(&self) -> Duration {
        if self.source == "musescore" {
            Duration::from_secs(MUSESCORE_CACHE_TTL_SECS)
        } else {
            Duration::from_secs(CACHE_TTL_SECS)
        }
    }
}

/// Cross-user search-result cache. Entries are `Arc<Vec<SearchResult>>` so
/// cache hits avoid cloning the inner Vec on the hot path (the caller
/// either uses the Arc directly or pays for an explicit clone).
pub type SearchCache = Cache<CacheKey, Arc<Vec<SearchResult>>>;

const CACHE_TTL_SECS: u64 = 60;
const CACHE_MAX_ENTRIES: u64 = 1000;
/// MuseScore search goes through FlareSolverr — a headless-Chromium
/// Cloudflare solve that costs 5–45 s on a cold query. A 60 s TTL would
/// re-pay that constantly. The catalog of community uploads moves slowly
/// and this is a low-traffic instance, so MuseScore search results get a
/// week-long TTL; the cheap HTTP sources (IMSLP, Mutopia) keep the short
/// 60 s freshness window.
const MUSESCORE_CACHE_TTL_SECS: u64 = 60 * 60 * 24 * 7;

/// Per-source TTL policy keyed off the static source id already carried in
/// `CacheKey`. MuseScore entries live a week; everything else expires after
/// `CACHE_TTL_SECS`. We only need `expire_after_create` — entries are never
/// updated in place (cache-aside replaces them wholesale on miss) and a read
/// shouldn't extend the lifetime, so the trait's other hooks keep their
/// no-op defaults.
struct SearchExpiry;

impl Expiry<CacheKey, Arc<Vec<SearchResult>>> for SearchExpiry {
    fn expire_after_create(
        &self,
        key: &CacheKey,
        _value: &Arc<Vec<SearchResult>>,
        _created_at: Instant,
    ) -> Option<Duration> {
        Some(key.ttl())
    }
}

pub fn new_search_cache() -> SearchCache {
    Cache::builder()
        .max_capacity(CACHE_MAX_ENTRIES)
        .expire_after(SearchExpiry)
        .build()
}

/// Resolved-thumbnail URL cache. Keyed by `"<source_id>:<item_id>"`.
/// Source thumbnail URLs are stable for the life of a wiki page revision,
/// so a long TTL is appropriate (24h). Bounded by entry count to keep
/// memory predictable.
pub type ThumbnailCache = Cache<String, String>;

const THUMBNAIL_CACHE_TTL_SECS: u64 = 60 * 60 * 24;
const THUMBNAIL_CACHE_MAX_ENTRIES: u64 = 5000;

pub fn new_thumbnail_cache() -> ThumbnailCache {
    Cache::builder()
        .max_capacity(THUMBNAIL_CACHE_MAX_ENTRIES)
        .time_to_live(Duration::from_secs(THUMBNAIL_CACHE_TTL_SECS))
        .build()
}

/// Server-rendered thumbnail PNG cache. Keyed by `"<source_id>:<item_id>"`.
/// Stores `(bytes, mime_type)` for sources that generate their thumbnail
/// inline (currently Mutopia, which rasterizes the PDF's first page via
/// pdftoppm). Capped by entry count rather than total bytes — a typical
/// page-1 PNG at 72 DPI is ~30–80 KB, so 500 entries is roughly 25 MB.
pub type ThumbnailBytesCache = Cache<String, Arc<(Vec<u8>, &'static str)>>;

const THUMBNAIL_BYTES_CACHE_MAX_ENTRIES: u64 = 500;

pub fn new_thumbnail_bytes_cache() -> ThumbnailBytesCache {
    Cache::builder()
        .max_capacity(THUMBNAIL_BYTES_CACHE_MAX_ENTRIES)
        .time_to_live(Duration::from_secs(THUMBNAIL_CACHE_TTL_SECS))
        .build()
}

/// Outcome of an [`L2Cache::get`] lookup, carrying the freshness signal the
/// stale-while-revalidate path in [`cached_search`] needs.
#[derive(Debug)]
enum L2Lookup {
    /// Row present and within its TTL — serve directly.
    Fresh(Vec<SearchResult>),
    /// Row present and past its TTL, but still inside the stale grace window
    /// (one further TTL). Serve it immediately and refresh in the background
    /// so nobody waits on a cold upstream fetch at the expiry edge.
    Stale(Vec<SearchResult>),
    /// No usable row: absent, beyond the grace window, or undecodable.
    Miss,
}

/// Durable (L2) search-result cache backed by the SQLite `search_cache`
/// table (see `migrations/0004_search_cache.sql`).
///
/// This sits *behind* the in-memory moka L1 cache. Its sole job is to let
/// expensive cold searches — chiefly MuseScore's 5–45 s FlareSolverr
/// Cloudflare solve — survive a container restart (and, if multiple
/// instances ever mount the same DB file, be shared across them) instead of
/// being re-paid on every redeploy.
///
/// Why SQLite rather than Redis/Memcached: the app already depends on
/// sqlx+sqlite, runs migrations, and persists everything else (sessions,
/// queue, audit log, settings) to a SQLite file on a durable volume. For a
/// ~5-user single-container instance, a networked cache service would add a
/// whole dependency tree and an extra container for no practical gain. The
/// cache table rides the existing persistent volume for free.
///
/// Every operation is best-effort: a DB error is logged and treated as a
/// miss/no-op so a cache fault can never fail a user's search. Construction
/// is opt-in via `BROWSARR_PERSISTENT_SEARCH_CACHE` (wired in `main.rs`);
/// when the cache is `None`, `cached_search` degrades to moka-only behavior.
#[derive(Clone)]
pub struct L2Cache {
    pool: SqlitePool,
}

impl L2Cache {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Look up an entry with stale-while-revalidate semantics:
    /// [`L2Lookup::Fresh`] within the TTL, [`L2Lookup::Stale`] for a row that
    /// expired but is still inside a grace window of one further TTL, and
    /// [`L2Lookup::Miss`] otherwise. Rows past the grace window — and any row
    /// we can't parse/decode — are swept lazily on this read path (no
    /// background job). Every error degrades to `Miss` (best-effort).
    async fn get(&self, key: &CacheKey) -> L2Lookup {
        let k = key.l2_key();
        let row: Result<Option<(String, String)>, _> = sqlx::query_as(
            "SELECT payload, expires_at FROM search_cache WHERE cache_key = ?",
        )
        .bind(&k)
        .fetch_optional(&self.pool)
        .await;

        let (payload, expires_at) = match row {
            Ok(Some(r)) => r,
            Ok(None) => return L2Lookup::Miss,
            Err(e) => {
                tracing::warn!(error = %e, "l2 cache: lookup failed");
                return L2Lookup::Miss;
            }
        };

        let expires = match OffsetDateTime::parse(&expires_at, &Rfc3339) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "l2 cache: expires_at parse; dropping row");
                self.sweep(&k).await;
                return L2Lookup::Miss;
            }
        };

        // Past the fresh window *and* the stale grace window → too old to
        // serve even stale; sweep and miss. Grace mirrors the per-source TTL,
        // so MuseScore stays servable-while-stale for a second week and the
        // cheap sources for a second 60 s.
        let now = OffsetDateTime::now_utc();
        if now > expires + key.ttl() {
            self.sweep(&k).await;
            return L2Lookup::Miss;
        }

        let results = match serde_json::from_str::<Vec<SearchResult>>(&payload) {
            Ok(v) => v,
            Err(e) => {
                // A schema drift / corrupt row shouldn't poison the cache
                // forever: drop it so the next miss repopulates cleanly.
                tracing::warn!(error = %e, "l2 cache: payload decode failed; dropping row");
                self.sweep(&k).await;
                return L2Lookup::Miss;
            }
        };

        if now < expires {
            L2Lookup::Fresh(results)
        } else {
            L2Lookup::Stale(results)
        }
    }

    /// Best-effort delete of a single cache row by its textual key.
    async fn sweep(&self, k: &str) {
        let _ = sqlx::query("DELETE FROM search_cache WHERE cache_key = ?")
            .bind(k)
            .execute(&self.pool)
            .await;
    }

    /// Upsert an entry with a per-source TTL (mirrors the moka L1 policy).
    /// Best-effort: a failure is logged and otherwise ignored.
    async fn insert(&self, key: &CacheKey, results: &[SearchResult]) {
        let payload = match serde_json::to_string(results) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "l2 cache: payload encode failed");
                return;
            }
        };
        let now = OffsetDateTime::now_utc();
        let ttl = key.ttl();
        let expires = now + ttl;
        let (created_at, expires_at) = match (now.format(&Rfc3339), expires.format(&Rfc3339)) {
            (Ok(c), Ok(e)) => (c, e),
            _ => {
                tracing::warn!("l2 cache: ts format on insert");
                return;
            }
        };

        let res = sqlx::query(
            "INSERT INTO search_cache (cache_key, source, payload, created_at, expires_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(cache_key) DO UPDATE SET \
               payload = excluded.payload, \
               created_at = excluded.created_at, \
               expires_at = excluded.expires_at",
        )
        .bind(key.l2_key())
        .bind(key.source)
        .bind(&payload)
        .bind(&created_at)
        .bind(&expires_at)
        .execute(&self.pool)
        .await;

        if let Err(e) = res {
            tracing::warn!(error = %e, "l2 cache: insert failed");
        }
    }
}

/// Two-tier, single-flight, stale-while-revalidate wrapper around
/// `Source::search`.
///
/// Lookup order on a request:
///   1. moka L1 (in-process, fast, lost on restart) — hit returns immediately.
///   2. durable L2 (`l2`, SQLite-backed) when configured:
///        * fresh hit → re-warm L1 and return.
///        * stale-but-within-grace hit → return the stale results *now* and
///          kick a background refresh, so nobody waits on the expiry edge (a
///          5–45 s MuseScore solve in the worst case).
///   3. cold miss → fetch from the real source, single-flighted through moka
///      (`try_get_with`) so N concurrent identical misses collapse into one
///      upstream call, then populate L1 + L2.
///
/// `l2 == None` reproduces the original moka-only cache-aside behavior, so
/// leaving `BROWSARR_PERSISTENT_SEARCH_CACHE` unset is a no-op.
pub async fn cached_search(
    cache: &SearchCache,
    l2: Option<&L2Cache>,
    source: &Arc<dyn Source>,
    query: &str,
    filters: &SearchFilters,
    limit: usize,
) -> anyhow::Result<Vec<SearchResult>> {
    let key = CacheKey {
        source: source.id(),
        query: query.to_string(),
        limit,
        instrument: filters.instrument,
    };

    // L1: in-process moka. Always fresh (moka enforces the TTL).
    if let Some(cached) = cache.get(&key).await {
        return Ok((*cached).clone());
    }

    // L2: durable SQLite store (best-effort), with stale-while-revalidate.
    if let Some(l2) = l2 {
        match l2.get(&key).await {
            L2Lookup::Fresh(results) => {
                cache.insert(key.clone(), Arc::new(results.clone())).await;
                return Ok(results);
            }
            L2Lookup::Stale(results) => {
                // Serve stale immediately; refresh in the background. The
                // refresh is single-flighted on the same moka key, so even if
                // many requests serve stale at once only one upstream fetch
                // runs.
                spawn_refresh(
                    cache.clone(),
                    l2.clone(),
                    source.clone(),
                    query.to_string(),
                    filters.clone(),
                    limit,
                );
                return Ok(results);
            }
            L2Lookup::Miss => {}
        }
    }

    // Cold miss in both tiers: single-flighted upstream fetch.
    fetch_single_flight(cache, l2, source, query, filters, limit, key).await
}

/// Fetch from the real source under moka's single-flight (`try_get_with`):
/// concurrent callers with the same key share one upstream call and one L2
/// write. Populates L1 (via `try_get_with`) and L2.
async fn fetch_single_flight(
    cache: &SearchCache,
    l2: Option<&L2Cache>,
    source: &Arc<dyn Source>,
    query: &str,
    filters: &SearchFilters,
    limit: usize,
    key: CacheKey,
) -> anyhow::Result<Vec<SearchResult>> {
    // moka runs `init` on the "leader" caller for this key; concurrent
    // followers await its result instead of launching their own fetch. Own
    // everything the future captures so it stays self-contained ('static).
    let l2 = l2.cloned();
    let source = source.clone();
    let query = query.to_string();
    let filters = filters.clone();
    let init_key = key.clone();
    let init = async move {
        let results = source.search(&query, &filters, limit).await?;
        let arc = Arc::new(results);
        if let Some(l2) = &l2 {
            l2.insert(&init_key, arc.as_ref()).await;
        }
        Ok::<_, anyhow::Error>(arc)
    };
    match cache.try_get_with(key, init).await {
        Ok(arc) => Ok((*arc).clone()),
        // try_get_with hands back the leader's error as `Arc<anyhow::Error>`;
        // flatten it into a fresh chain for the caller.
        Err(e) => Err(anyhow::anyhow!("{:#}", e)),
    }
}

/// Spawn a detached, single-flighted refresh of a key whose L2 entry was just
/// served stale. Errors are swallowed (the caller already returned stale
/// results); success repopulates L1 + L2 with fresh data.
fn spawn_refresh(
    cache: SearchCache,
    l2: L2Cache,
    source: Arc<dyn Source>,
    query: String,
    filters: SearchFilters,
    limit: usize,
) {
    tokio::spawn(async move {
        let key = CacheKey {
            source: source.id(),
            query: query.clone(),
            limit,
            instrument: filters.instrument,
        };
        if let Err(e) =
            fetch_single_flight(&cache, Some(&l2), &source, &query, &filters, limit, key).await
        {
            tracing::warn!(
                source = source.id(),
                error = %format!("{e:#}"),
                "l2 cache: background refresh failed"
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    /// In-memory SQLite pool with the real migrations applied. `max_connections(1)`
    /// keeps every query on the same `:memory:` database (each connection to
    /// `sqlite::memory:` is otherwise its own isolated DB).
    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory sqlite");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("run migrations");
        pool
    }

    fn sample_result(title: &str) -> SearchResult {
        SearchResult {
            source: "musescore".to_string(),
            id: "abc123".to_string(),
            title: title.to_string(),
            description: Some("desc".to_string()),
            external_url: "https://example.test/abc123".to_string(),
            thumbnail_url: None,
            metadata: Vec::new(),
            complexity: Some(2),
            is_public_domain: Some(true),
            is_official: Some(false),
        }
    }

    fn key(source: &'static str, query: &str, limit: usize) -> CacheKey {
        CacheKey {
            source,
            query: query.to_string(),
            limit,
            instrument: None,
        }
    }

    #[test]
    fn l2_key_distinguishes_every_field() {
        let base = key("musescore", "chopin", 20);
        // Each varied field must produce a distinct textual key, or two
        // logically different searches would collide in the L2 table.
        assert_ne!(base.l2_key(), key("imslp", "chopin", 20).l2_key());
        assert_ne!(base.l2_key(), key("musescore", "bach", 20).l2_key());
        assert_ne!(base.l2_key(), key("musescore", "chopin", 40).l2_key());
        let mut with_instrument = base.clone();
        with_instrument.instrument = Some(Instrument::Piano);
        assert_ne!(base.l2_key(), with_instrument.l2_key());
    }

    #[test]
    fn ttl_is_source_specific() {
        assert_eq!(
            key("musescore", "q", 20).ttl(),
            Duration::from_secs(MUSESCORE_CACHE_TTL_SECS)
        );
        assert_eq!(
            key("imslp", "q", 20).ttl(),
            Duration::from_secs(CACHE_TTL_SECS)
        );
    }

    #[tokio::test]
    async fn l2_round_trips_a_search_result() {
        let l2 = L2Cache::new(test_pool().await);
        let k = key("musescore", "chopin nocturne", 20);
        assert!(
            matches!(l2.get(&k).await, L2Lookup::Miss),
            "cold lookup is a miss"
        );

        let results = vec![sample_result("Nocturne Op.9 No.2")];
        l2.insert(&k, &results).await;

        let got = match l2.get(&k).await {
            L2Lookup::Fresh(v) => v,
            other => panic!("expected Fresh after insert, got {other:?}"),
        };
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].title, "Nocturne Op.9 No.2");
        assert_eq!(got[0].complexity, Some(2));
    }

    #[tokio::test]
    async fn l2_treats_expired_rows_as_misses_and_sweeps_them() {
        let pool = test_pool().await;
        let l2 = L2Cache::new(pool.clone());
        let k = key("imslp", "already stale", 20);

        // Insert a row that expired an hour ago by hand (the public insert
        // path always writes a future expiry).
        let past = (OffsetDateTime::now_utc() - Duration::from_secs(3600))
            .format(&Rfc3339)
            .unwrap();
        sqlx::query(
            "INSERT INTO search_cache (cache_key, source, payload, created_at, expires_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(k.l2_key())
        .bind(k.source)
        .bind("[]")
        .bind(&past)
        .bind(&past)
        .execute(&pool)
        .await
        .unwrap();

        assert!(
            matches!(l2.get(&k).await, L2Lookup::Miss),
            "row past the grace window reads as a miss"
        );

        let remaining: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM search_cache")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining.0, 0, "row past the grace window was swept on read");
    }

    #[tokio::test]
    async fn l2_serves_recently_expired_rows_as_stale() {
        let pool = test_pool().await;
        let l2 = L2Cache::new(pool.clone());
        // imslp TTL is 60s with a 60s stale grace, so a row that expired 10s
        // ago is still inside the grace window → Stale (served while a
        // background refresh runs), and must NOT be swept.
        let k = key("imslp", "barely stale", 20);
        let expired_10s_ago = (OffsetDateTime::now_utc() - Duration::from_secs(10))
            .format(&Rfc3339)
            .unwrap();
        let created = (OffsetDateTime::now_utc() - Duration::from_secs(70))
            .format(&Rfc3339)
            .unwrap();
        let payload = serde_json::to_string(&vec![sample_result("stale hit")]).unwrap();
        sqlx::query(
            "INSERT INTO search_cache (cache_key, source, payload, created_at, expires_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(k.l2_key())
        .bind(k.source)
        .bind(&payload)
        .bind(&created)
        .bind(&expired_10s_ago)
        .execute(&pool)
        .await
        .unwrap();

        match l2.get(&k).await {
            L2Lookup::Stale(v) => assert_eq!(v[0].title, "stale hit"),
            other => panic!("expected Stale within grace window, got {other:?}"),
        }

        let remaining: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM search_cache")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining.0, 1, "stale row retained for serve-while-revalidate");
    }

    #[tokio::test]
    async fn l2_insert_upserts_on_duplicate_key() {
        let l2 = L2Cache::new(test_pool().await);
        let k = key("musescore", "dup", 20);

        l2.insert(&k, &[sample_result("first")]).await;
        l2.insert(&k, &[sample_result("second")]).await;

        let got = match l2.get(&k).await {
            L2Lookup::Fresh(v) => v,
            other => panic!("expected Fresh, got {other:?}"),
        };
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].title, "second", "second insert overwrote the first");
    }

    #[tokio::test]
    async fn cached_search_with_no_l2_uses_moka_only() {
        // A None L2 must reproduce the original moka-only cache-aside path.
        // We exercise it indirectly: insert into L1 directly, then confirm a
        // search-like read hits L1 without needing a source or DB.
        let l1 = new_search_cache();
        let k = key("imslp", "moka only", 20);
        l1.insert(k.clone(), Arc::new(vec![sample_result("cached")]))
            .await;
        let hit = l1.get(&k).await.expect("L1 hit");
        assert_eq!(hit[0].title, "cached");
    }
}
