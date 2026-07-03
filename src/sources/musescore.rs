use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use async_trait::async_trait;
use futures_util::StreamExt;
use printpdf::{Mm, Op, PdfDocument, PdfPage, PdfSaveOptions, RawImage, XObjectTransform};
use reqwest::cookie::Jar;
use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Url};
use scraper::{Html, Selector};
use serde::Deserialize;
use tokio::sync::Mutex;

use super::flaresolverr::{FlareSolverr, FsError, FsSolution};
use super::{BadgeKind, MetadataBadge, SearchFilters, SearchResult, Source};

/// Env var name. MuseScore fetches now try a direct browser-style request
/// first (dl-librescore's method); when set, this is the FlareSolverr endpoint
/// we fall back to *only if* a direct score-page / `/sheetmusic` search fetch
/// comes back as a Cloudflare challenge. Bundle JS, /api/jmuse, and CDN PNG
/// fetches are always direct (they replay any harvested `cf_clearance` + UA).
/// Leave it unset to run pure-direct — correct when the host's egress isn't
/// Cloudflare-challenged (e.g. a residential IP), the way dl-librescore runs.
const FLARESOLVERR_ENV: &str = "FLARESOLVERR_URL";

/// Env var controlling the FlareSolverr session pool size. Each session
/// holds a long-lived Chromium browser context on the FS side, so
/// `N` sessions = up to `N` parallel solves (FS internally serializes
/// per session). 3 is a sensible default for a typical homelab FS
/// container: enough to cover the 4-CJK-variant fan-out with some
/// headroom, without paying for browsers we never use. Bumping past
/// ~6 starts to stress FS's RAM budget; lowering to 1 mirrors the
/// pre-pool single-session behavior.
const FLARESOLVERR_POOL_ENV: &str = "FLARESOLVERR_POOL_SIZE";
const FLARESOLVERR_POOL_DEFAULT: usize = 3;

/// How often the background task destroys + recreates every session in
/// the pool. FlareSolverr's bundled Chromium has a slow memory leak
/// over days of uptime; recycling daily keeps each browser fresh
/// without disrupting steady-state traffic (refresh is serial, so at
/// most one pool slot is unavailable at any moment).
const FS_SESSION_REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 3600);

// MuseScore.com sits behind Cloudflare; stale or obvious-bot UAs get the
// "Just a moment…" challenge page (HTTP 403). The full set of headers a
// modern desktop Chrome sends on a top-level navigation is what gets us
// past the basic bot check. Headers safe to send on every request live in
// `default_headers()`; navigation-only ones (Sec-Fetch-*,
// Upgrade-Insecure-Requests) are layered per-request via `nav_headers()`
// so the bundle JS / jmuse XHR / CDN PNG fetches don't misrepresent
// themselves.
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
const ACCEPT_LANGUAGE: &str = "en-US,en;q=0.9";
const ACCEPT_DOC: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7";
const SEC_CH_UA: &str =
    "\"Chromium\";v=\"140\", \"Not?A_Brand\";v=\"24\", \"Google Chrome\";v=\"140\"";
const SEC_CH_UA_PLATFORM: &str = "\"Windows\"";

const TIMEOUT: Duration = Duration::from_secs(20);

/// Cadence for the background `cf_clearance` keep-warm re-solve. Cloudflare's
/// cookie TTL is site-configured and opaque to us, but the common default is
/// ~30 min; re-solving every 15 min mints a fresh cookie comfortably before a
/// 30-min one lapses, so real user requests keep landing on the fast
/// direct-replay path instead of a cold FlareSolverr solve.
const KEEP_WARM_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// A search/fetch must have happened within this window for the keep-warm
/// loop to bother re-solving. Bounds idle FlareSolverr usage: once traffic
/// stops for this long we let the cookie lapse and the next user eats a
/// single cold solve, rather than burning FS cycles on an idle instance.
const KEEP_WARM_ACTIVE_WINDOW: Duration = Duration::from_secs(30 * 60);

/// Cheap, reliably CF-challenged URL used to mint/refresh `cf_clearance` out
/// of band (startup warm-up and keep-warm loop).
const WARM_URL: &str = "https://musescore.com/sheetmusic";

/// Bounded concurrency for per-page CDN PNG downloads. The image CDN (unlike
/// the rate-limited `/api/jmuse`) tolerates parallel fetches, so this cuts
/// the download phase of a multi-page score roughly linearly while staying
/// polite.
const PNG_FETCH_CONCURRENCY: usize = 4;

/// Hardcoded fallback salt for the jmuse MD5 token, mirroring the value
/// committed in LibreScore/dl-librescore's `src/file.ts` (there:
/// `md5(`${id}${type}${index}9654,4e`).slice(0, 4)`). MuseScore's per-deploy
/// salt lives in the JS bundle and we extract it at runtime, but that
/// extraction is the most fragile stage (bundle chunk renames, minifier
/// churn). When the extracted salt's token is rejected — or no salt could be
/// extracted at all — we retry with this known-good value before giving up.
/// It changes rarely; when it does, dl-librescore's repo is the place to
/// pick up the new one.
const FALLBACK_SALT: &str = "9654,4e";

/// Per-request headers a real Chrome sends on a top-level navigation
/// (typed URL / link click). Cloudflare's bot heuristics weight these
/// heavily, especially `Sec-Fetch-Site: none` (i.e., not from a referrer).
/// Layered on top of the Client's `default_headers` for score-page and
/// search-page fetches only.
fn nav_headers() -> [(HeaderName, HeaderValue); 6] {
    [
        (header::ACCEPT, HeaderValue::from_static(ACCEPT_DOC)),
        (
            header::UPGRADE_INSECURE_REQUESTS,
            HeaderValue::from_static("1"),
        ),
        (
            HeaderName::from_static("sec-fetch-dest"),
            HeaderValue::from_static("document"),
        ),
        (
            HeaderName::from_static("sec-fetch-mode"),
            HeaderValue::from_static("navigate"),
        ),
        (
            HeaderName::from_static("sec-fetch-site"),
            HeaderValue::from_static("none"),
        ),
        (
            HeaderName::from_static("sec-fetch-user"),
            HeaderValue::from_static("?1"),
        ),
    ]
}

/// MuseScore.com — community-uploaded sheet music. The site does not expose
/// a server-side PDF for user uploads (`is_pdf == 0` for ~all free scores);
/// instead it serves per-page PNG renderings through an authenticated API
/// (`/api/jmuse`) that requires a short-lived token: the first 4 hex chars of
/// `md5(id + type + index + salt)`, where `salt` is a short string embedded
/// in their webpack bundle. We port the technique from the
/// `musescore-downloader` browser extension / yt-dlp:
///
///   1. Fetch a score page and extract the bundle URL from a `<link>` tag.
///   2. Download the bundle (~0.5 MB minified JS) and extract the `salt`
///      string literal (the one fed into `md5(…).substr(0, 4)`).
///   3. For each page index 0..pages_count, compute the MD5 token natively
///      (no JS engine), call jmuse for the `type=img` CDN URL, GET the PNG.
///   4. Stitch PNGs into a single PDF (printpdf) and return the bytes.
///
/// The salt is cached by bundle URL; bundle URLs change on every MuseScore
/// deploy (the path embeds a content hash), so a single cache entry is
/// sufficient — when MuseScore deploys, we re-extract the salt once.
pub struct Musescore {
    http: Client,
    /// Shared cookie jar — anything Cloudflare hands us (`cf_clearance`,
    /// `__cf_bm`, etc.) gets stashed here automatically by reqwest on
    /// direct fetches, and gets injected explicitly from FlareSolverr's
    /// solution payload after a challenge-solved fetch. Either way the
    /// cookies are replayed on subsequent direct calls (bundle JS,
    /// /api/jmuse, CDN PNGs).
    jar: Arc<Jar>,
    /// Optional FlareSolverr proxy. Some(_) when `FLARESOLVERR_URL` is
    /// set at startup; None otherwise. `fetch_html_challenged()` routes
    /// through it when present and falls back to direct otherwise.
    fs: Option<FlareSolverr>,
    /// Pool of lazily-created FS session IDs. Each slot is a long-lived
    /// Chromium browser context on the FS side. FS serializes calls
    /// per session, so parallelism scales with pool size: with 3 slots
    /// the 4-CJK-variant search fan-out lands on 3 different sessions
    /// concurrently (4th waits behind one), versus serializing on a
    /// single session pre-pool. A slot is None until first use OR
    /// after invalidation (FS reported session-missing, refresh task
    /// destroyed it); the next acquire on that slot recreates it. Empty
    /// vec when `fs` is None — keeps the no-FS code path cost-free.
    fs_sessions: Vec<Mutex<Option<String>>>,
    /// Round-robin cursor into `fs_sessions`. Incremented on every
    /// acquire and reduced mod pool size. Relaxed ordering is fine —
    /// occasional slot-skipping under contention doesn't affect
    /// correctness, just fairness, and the pool is small enough that
    /// drift evens out within a handful of requests.
    fs_session_cursor: AtomicUsize,
    /// The User-Agent FlareSolverr's bundled Chromium reported on the most
    /// recent successful solve. `cf_clearance` is bound to the (IP, UA)
    /// tuple, so a direct cookie-replay fetch MUST send this exact UA or
    /// Cloudflare re-challenges. `None` until the first FS solve lands;
    /// once set, `fetch_html_challenged` tries a plain reqwest GET first
    /// (replaying the harvested `cf_clearance` under this UA) and only
    /// falls back to FlareSolverr when that cookie has expired — turning
    /// the steady-state search/score-page fetch from a multi-second
    /// headless-Chromium round-trip into a sub-second HTTP call.
    fs_ua: Mutex<Option<String>>,
    /// Single-flight gate around the FlareSolverr solve. The i18n layer
    /// fans one user query into up to 4 CJK variants that reach
    /// `fetch_html_challenged` in parallel; the search-cache single-flight
    /// keys on `(source, variant)` so it does NOT coalesce them, and at
    /// every cold-start / cookie-expiry boundary each variant would
    /// otherwise launch its own 5–30 s headless-Chromium solve. Serializing
    /// the *decision to solve* lets the first caller mint the `cf_clearance`;
    /// the rest wake, replay it via `try_direct`, and skip FlareSolverr
    /// entirely. Only the leader pays the solve cost.
    fs_solve_lock: Mutex<()>,
    cached: Mutex<Option<CachedAlgorithm>>,
    /// Unix timestamp (seconds) of the last user-driven search / PDF fetch,
    /// or 0 if none yet. Read by the keep-warm loop to decide whether the
    /// instance is active enough to justify a background re-solve.
    last_activity: AtomicI64,
}

struct CachedAlgorithm {
    bundle_url: String,
    /// The `salt` string MuseScore concatenates into the jmuse MD5 token.
    /// Changes per deploy; re-extracted when the bundle URL changes.
    random_token: String,
}

impl Musescore {
    pub fn new() -> anyhow::Result<Self> {
        // Headers safe to send on every request type (navigation, XHR,
        // script, CDN). Per-request navigation-only headers are added on
        // top by callers that need them via `nav_headers()`.
        let mut default_headers = HeaderMap::new();
        default_headers.insert(
            header::ACCEPT_LANGUAGE,
            HeaderValue::from_static(ACCEPT_LANGUAGE),
        );
        default_headers.insert(
            HeaderName::from_static("sec-ch-ua"),
            HeaderValue::from_static(SEC_CH_UA),
        );
        default_headers.insert(
            HeaderName::from_static("sec-ch-ua-mobile"),
            HeaderValue::from_static("?0"),
        );
        default_headers.insert(
            HeaderName::from_static("sec-ch-ua-platform"),
            HeaderValue::from_static(SEC_CH_UA_PLATFORM),
        );

        let jar = Arc::new(Jar::default());
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(TIMEOUT)
            .gzip(true)
            .default_headers(default_headers)
            .cookie_provider(jar.clone())
            .build()?;

        // Opt-in FlareSolverr wiring. We log once at startup so the
        // operator can confirm the env var was picked up; further FS
        // failures are logged at the call site.
        let fs = match std::env::var(FLARESOLVERR_ENV).ok().filter(|s| !s.is_empty()) {
            Some(url) => {
                tracing::info!(flaresolverr_url = %url, "MuseScore: routing CF-challenged requests through FlareSolverr");
                Some(FlareSolverr::new(url).context("constructing FlareSolverr client")?)
            }
            None => {
                tracing::debug!("MuseScore: FLARESOLVERR_URL unset; direct fetches only");
                None
            }
        };

        // Pool size is read once at startup; changing it requires a
        // container restart. Cap at 1 — a zero-sized pool would mean
        // "FS is configured but unusable", which the caller can't
        // distinguish from "no FS at all" without extra branches.
        let pool_size = std::env::var(FLARESOLVERR_POOL_ENV)
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .map(|n| n.max(1))
            .unwrap_or(FLARESOLVERR_POOL_DEFAULT);
        let fs_sessions: Vec<Mutex<Option<String>>> = if fs.is_some() {
            tracing::info!(pool_size, "MuseScore: FlareSolverr session pool configured");
            (0..pool_size).map(|_| Mutex::new(None)).collect()
        } else {
            Vec::new()
        };

        Ok(Self {
            http,
            jar,
            fs,
            fs_sessions,
            fs_session_cursor: AtomicUsize::new(0),
            fs_ua: Mutex::new(None),
            fs_solve_lock: Mutex::new(()),
            cached: Mutex::new(None),
            last_activity: AtomicI64::new(0),
        })
    }

    /// Spawn the out-of-band cookie warm-up tasks. No-op when FlareSolverr
    /// isn't configured (direct fetches need no `cf_clearance` management).
    /// Two tasks:
    ///   * **startup warm-up** — one solve at boot so the *first* user request
    ///     after a (re)deploy lands on the fast direct-replay path instead of
    ///     paying a cold 30–60 s FlareSolverr solve. The in-memory cookie jar
    ///     starts empty on every restart, so without this the first searcher
    ///     always eats the cold cost.
    ///   * **keep-warm loop** — while the source has seen recent traffic,
    ///     re-solve every `KEEP_WARM_INTERVAL` to mint a fresh `cf_clearance`
    ///     before the current one expires, holding the steady state on the
    ///     fast path. Skips re-solving once the instance goes idle.
    ///
    /// Takes `Arc<Self>` so the detached tasks can outlive this call.
    pub fn spawn_warm_tasks(self: Arc<Self>) {
        if self.fs.is_none() {
            return;
        }

        let startup = Arc::clone(&self);
        tokio::spawn(async move {
            match startup.force_warm().await {
                Ok(()) => tracing::info!("MuseScore: cf_clearance warmed at startup"),
                Err(e) => tracing::warn!(
                    error = %format!("{:#}", e),
                    "MuseScore startup warm-up failed; first request will solve on demand"
                ),
            }
        });

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(KEEP_WARM_INTERVAL);
            // Drop the immediate first tick — startup already warmed the cookie.
            ticker.tick().await;
            let active_window = KEEP_WARM_ACTIVE_WINDOW.as_secs() as i64;
            loop {
                ticker.tick().await;
                let idle = self.seconds_since_activity();
                if idle > active_window {
                    tracing::debug!(idle_secs = idle, "MuseScore keep-warm: idle, skipping re-solve");
                    continue;
                }
                match self.force_warm().await {
                    Ok(()) => tracing::debug!("MuseScore keep-warm: cf_clearance refreshed"),
                    Err(e) => tracing::warn!(
                        error = %format!("{:#}", e),
                        "MuseScore keep-warm re-solve failed"
                    ),
                }
            }
        });
    }

    /// Spawn the background task that refreshes every FS session in the
    /// pool. Runs the first sweep immediately (so the pool is pre-warmed
    /// — first user search doesn't pay the create cost), then sleeps
    /// 24h between sweeps. No-op when FS isn't configured.
    ///
    /// Complementary to `spawn_warm_tasks`: that one keeps the
    /// `cf_clearance` cookie hot so user requests stay on the fast
    /// direct-replay path. This one keeps the FS-side Chromium
    /// browser contexts hot so when we *do* need to solve, we don't
    /// pay browser cold-start. Both no-op when FS isn't wired.
    ///
    /// Takes `Arc<Self>` so the spawned task can outlive the caller's
    /// reference. Caller is expected to keep its own `Arc` (the source
    /// registry does this already), so we don't bother with a weak ref:
    /// when the process shuts down, tokio drops the task with the
    /// runtime.
    pub fn spawn_session_refresh(self: Arc<Self>) {
        if self.fs.is_none() || self.fs_sessions.is_empty() {
            return;
        }
        tokio::spawn(async move {
            // Pre-warm immediately. We hit each slot serially rather
            // than racing them so FS isn't asked to launch N browsers
            // simultaneously at startup (it handles that, but it's
            // wasteful — staggering by a few seconds is gentler and
            // any in-flight user request can land on an already-warm
            // earlier slot while later ones are still booting).
            for idx in 0..self.fs_sessions.len() {
                self.refresh_one_session(idx).await;
            }
            loop {
                tokio::time::sleep(FS_SESSION_REFRESH_INTERVAL).await;
                tracing::info!("FlareSolverr session refresh: nightly sweep starting");
                for idx in 0..self.fs_sessions.len() {
                    self.refresh_one_session(idx).await;
                }
                tracing::info!("FlareSolverr session refresh: nightly sweep complete");
            }
        });
    }

    /// Force a fresh `cf_clearance` by solving through FlareSolverr,
    /// deliberately bypassing the direct-replay fast path (a still-valid but
    /// aging cookie would otherwise short-circuit and never get refreshed).
    /// Harvests the new cookie + UA into the shared jar exactly like a normal
    /// challenged fetch. No-op when FlareSolverr isn't configured.
    ///
    /// Uses one slot from the session pool via `acquire_session` — keep-warm
    /// is best-effort, so we don't bother with the user-path retry on
    /// `SessionMissing`: a transient session-missing here just means the
    /// next 15-min tick will succeed on a different (or freshly recreated)
    /// slot.
    async fn force_warm(&self) -> anyhow::Result<()> {
        let Some(fs) = self.fs.as_ref() else {
            return Ok(());
        };
        let session = self.acquire_session().await;
        let solution = fs
            .get(WARM_URL, session.as_deref())
            .await
            .context("flaresolverr warm-up solve")?;
        anyhow::ensure!(
            solution.status < 400,
            "flaresolverr warm-up HTTP {}",
            solution.status
        );
        self.absorb_fs_cookies(&solution);
        self.remember_fs_ua(&solution.user_agent).await;
        Ok(())
    }

    /// Record that a user-driven operation just ran, for the keep-warm loop.
    fn mark_activity(&self) {
        self.last_activity.store(now_unix(), Ordering::Relaxed);
    }

    /// Seconds since the last user-driven operation. Large when there's been
    /// no traffic (or none since boot), which the keep-warm loop reads as
    /// "idle, don't bother re-solving".
    fn seconds_since_activity(&self) -> i64 {
        now_unix().saturating_sub(self.last_activity.load(Ordering::Relaxed))
    }

    /// Destroy the session currently in slot `idx` (if any) and create
    /// a fresh one in its place. Holds the slot's mutex throughout, so
    /// any concurrent acquire waits and then sees the new session ID —
    /// no torn reads. The slot stays unavailable for the duration of
    /// one destroy + one create call (~2–10 s); pool-mates handle
    /// traffic in the meantime.
    async fn refresh_one_session(&self, idx: usize) {
        let Some(fs) = self.fs.as_ref() else { return };
        let Some(slot) = self.fs_sessions.get(idx) else {
            return;
        };
        let mut guard = slot.lock().await;
        if let Some(old) = guard.take() {
            if let Err(e) = fs.destroy_session(&old).await {
                // Best-effort. A failed destroy mostly means FS doesn't
                // know about the session anymore (restart, manual purge),
                // which is what we wanted anyway.
                tracing::debug!(
                    session = %old,
                    error = %format!("{:#}", e),
                    "FlareSolverr session destroy failed during refresh; continuing"
                );
            }
        }
        let new_id = format!("musescore-{idx}");
        match fs.create_session(&new_id).await {
            Ok(()) => {
                tracing::info!(session = %new_id, "FlareSolverr session created");
                *guard = Some(new_id);
            }
            Err(e) => {
                tracing::warn!(
                    idx,
                    error = %format!("{:#}", e),
                    "FlareSolverr session create failed; slot left empty (will retry on next acquire)"
                );
            }
        }
    }

    /// Pick a session from the pool using round-robin and return its
    /// ID, creating one on demand if this slot is empty (first hit, or
    /// a prior `invalidate_session` cleared it). Returns `None` when
    /// FS isn't configured or the create attempt failed — caller
    /// degrades to sessionless mode in that case.
    ///
    /// The mutex is held across the create call so concurrent first-
    /// callers for the same slot serialize on each other. With a pool
    /// of size N, you can still have up to N create calls in flight
    /// concurrently (one per slot), but you won't issue 4 redundant
    /// creates for the same slot under a burst.
    async fn acquire_session(&self) -> Option<String> {
        let fs = self.fs.as_ref()?;
        if self.fs_sessions.is_empty() {
            return None;
        }
        let idx = self.fs_session_cursor.fetch_add(1, Ordering::Relaxed) % self.fs_sessions.len();
        let slot = &self.fs_sessions[idx];
        let mut guard = slot.lock().await;
        if let Some(s) = guard.as_ref() {
            return Some(s.clone());
        }
        let session_id = format!("musescore-{idx}");
        match fs.create_session(&session_id).await {
            Ok(()) => {
                tracing::info!(session = %session_id, "FlareSolverr session created (on-demand)");
                *guard = Some(session_id.clone());
                Some(session_id)
            }
            Err(e) => {
                tracing::warn!(
                    idx,
                    error = %format!("{:#}", e),
                    "FlareSolverr session create failed; falling back to sessionless mode for this request"
                );
                None
            }
        }
    }

    /// Null out whichever pool slot is currently holding `session`. Called
    /// after FS reports `SessionMissing` so the next acquire for that slot
    /// recreates it, instead of the caller repeatedly trying a dead ID.
    /// O(N) over the small pool — cheaper than mapping session → slot at
    /// the cost of keeping the data structure flat.
    async fn invalidate_session(&self, session: &str) {
        for (idx, slot) in self.fs_sessions.iter().enumerate() {
            let mut guard = slot.lock().await;
            if guard.as_deref() == Some(session) {
                tracing::info!(
                    session,
                    idx,
                    "FlareSolverr session invalidated; will recreate on next use"
                );
                *guard = None;
                return;
            }
        }
    }

    /// Fetch a Cloudflare-gated MuseScore HTML page (score page or search
    /// page), **preferring a direct browser-style request** — dl-librescore's
    /// method. From a residential egress Cloudflare usually doesn't challenge,
    /// so the direct fetch returns the page and FlareSolverr is never touched.
    /// Only when the direct fetch comes back as a challenge (or otherwise fails
    /// to yield the page) do we fall back to FlareSolverr, if configured;
    /// cookies it captures are injected into our shared jar so later direct
    /// calls (bundle JS, /api/jmuse, CDN PNGs) can replay the `cf_clearance`.
    async fn fetch_html_challenged(&self, url: &str, ctx_label: &'static str) -> anyhow::Result<String> {
        // Primary path (dl-librescore): a direct browser fetch.
        let direct_miss = match self.try_direct(url, ctx_label).await {
            Ok(html) => return Ok(html),
            Err(reason) => reason,
        };

        let Some(fs) = self.fs.as_ref() else {
            // No FlareSolverr fallback wired — the direct fetch is all we have.
            anyhow::bail!(
                "musescore {ctx_label}: direct fetch did not return the page ({direct_miss}); \
                 no FlareSolverr configured — set FLARESOLVERR_URL or route egress through an \
                 unchallenged proxy ({url})"
            );
        };

        tracing::debug!(
            ctx = ctx_label,
            reason = %direct_miss,
            "musescore: direct fetch challenged; falling back to FlareSolverr"
        );

        // Coalesce concurrent solvers: the i18n fan-out lands up to 4 variants
        // here at once when direct is challenged, but only one needs to drive
        // FlareSolverr. Hold the solve gate, then re-check the direct path — a
        // peer solve that landed while we were queued has already refreshed the
        // jar + UA, so our replay now succeeds and we skip the redundant
        // headless-Chromium round-trip. The guard stays held through the
        // absorb/remember below so waiters only wake once the fresh cookie + UA
        // are actually in place.
        let _solve_guard = self.fs_solve_lock.lock().await;
        if let Ok(html) = self.try_direct(url, ctx_label).await {
            return Ok(html);
        }

        // Two-shot loop: try a session, retry once on missing. Any other FS
        // error bails on the first attempt.
        for attempt in 0..2 {
            let session = self.acquire_session().await;
            match fs.get(url, session.as_deref()).await {
                Ok(solution) => {
                    if solution.status >= 400 {
                        anyhow::bail!(
                            "flaresolverr {ctx_label} HTTP {}: {}",
                            solution.status,
                            truncate_for_log(&solution.response, 200)
                        );
                    }
                    self.absorb_fs_cookies(&solution);
                    self.remember_fs_ua(&solution.user_agent).await;
                    return Ok(solution.response);
                }
                Err(FsError::SessionMissing { session: gone }) if attempt == 0 => {
                    tracing::info!(
                        session = %gone,
                        ctx = ctx_label,
                        "FlareSolverr reported session missing; invalidating slot and retrying"
                    );
                    self.invalidate_session(&gone).await;
                    continue;
                }
                Err(e) => {
                    return Err(anyhow::Error::new(e))
                        .with_context(|| format!("flaresolverr {ctx_label} {url}"));
                }
            }
        }
        // Loop only exits via continue (one retry permitted) or an early
        // return; reaching this line means we retried and hit SessionMissing
        // twice in a row, which suggests FS itself is unhealthy.
        anyhow::bail!(
            "flaresolverr {ctx_label} {url}: session missing twice in a row (FS may be unhealthy)"
        );
    }

    /// Attempt a direct browser-style fetch of a Cloudflare-gated page —
    /// dl-librescore's primary method. Sends full navigation headers and, when
    /// we've learned one from a prior FlareSolverr solve, the `cf_clearance`-
    /// bound User-Agent (the cookie itself rides along automatically from the
    /// jar); otherwise the Client's default browser UA. Returns `Ok(html)` only
    /// on a clean 2xx that isn't a Cloudflare interstitial. `Err(reason)` means
    /// "didn't get the page" — a challenge, a non-2xx, or a transport error;
    /// the caller decides whether to fall back to FlareSolverr. The reason
    /// string feeds the debug log and the no-FS error message.
    ///
    /// Unlike the old clearance-only fast path, this runs even before we've
    /// ever solved (cold), so a residential egress that Cloudflare never
    /// challenges resolves every page here without FlareSolverr.
    async fn try_direct(&self, url: &str, ctx_label: &'static str) -> Result<String, String> {
        // Replay the cf_clearance-bound UA if we have one; else the Client's
        // default browser UA. `nav_headers` make this look like a top-level
        // navigation, which Cloudflare's heuristics weight heavily.
        let ua = self.fs_ua.lock().await.clone();
        let mut req = self.http.get(url);
        if let Some(ua) = ua {
            req = req.header(header::USER_AGENT, ua);
        }
        for (k, v) in nav_headers() {
            req = req.header(k, v);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => return Err(format!("transport error: {e}")),
        };
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| format!("body read error: {e}"))?;
        if !status.is_success() {
            return Err(format!("HTTP {status}: {}", truncate_for_log(&body, 160)));
        }
        if looks_like_cf_challenge(&body) {
            return Err("Cloudflare challenge interstitial".to_string());
        }
        tracing::debug!(ctx = ctx_label, "musescore: direct fetch hit (no FlareSolverr needed)");
        Ok(body)
    }

    /// Remember the UA FlareSolverr's Chromium used on a successful solve so
    /// the next `try_direct` replays `cf_clearance` under the same UA. Empty
    /// UAs (older FS builds occasionally omit the field) are ignored so we
    /// don't poison the direct path with a blank header.
    async fn remember_fs_ua(&self, ua: &str) {
        if ua.is_empty() {
            return;
        }
        let mut guard = self.fs_ua.lock().await;
        if guard.as_deref() != Some(ua) {
            *guard = Some(ua.to_string());
        }
    }

    /// Inject every cookie FlareSolverr captured back into our reqwest
    /// cookie jar. Reqwest enforces domain / path / Secure scoping at
    /// match-time, so we serialize each cookie as a Set-Cookie-style
    /// string scoped to its own domain and let the jar do the right
    /// thing for subsequent direct requests.
    fn absorb_fs_cookies(&self, solution: &FsSolution) {
        // `Jar::add_cookie_str` wants a URL whose scheme + host imply the
        // cookie's domain. musescore.com (and its subdomains) is the only
        // host we care about; if FS ever returns cookies for a different
        // origin we'd be storing them too narrowly, but that's harmless.
        let Ok(base) = Url::parse("https://musescore.com/") else {
            return;
        };
        for c in &solution.cookies {
            let path = if c.path.is_empty() { "/" } else { c.path.as_str() };
            let secure = if c.secure { "; Secure" } else { "" };
            // Strip a leading dot from the domain — `Set-Cookie: Domain=` may
            // begin with one (RFC 6265 historical quirk) but reqwest's parser
            // prefers it without.
            let domain = c.domain.trim_start_matches('.');
            let cookie_str = format!(
                "{}={}; Domain={}; Path={}{}",
                c.name, c.value, domain, path, secure
            );
            self.jar.add_cookie_str(&cookie_str, &base);
        }
    }

    /// Snapshot the User-Agent our most recent FlareSolverr solve reported.
    /// `cf_clearance` is bound to the (IP, UA) tuple, so every *direct*
    /// request that must satisfy Cloudflare (bundle JS, `/api/jmuse`, CDN
    /// image) has to replay this exact UA or the cookie is rejected and we
    /// get a challenge page instead of our payload. `None` when FlareSolverr
    /// was never configured / has never solved — in that mode there's no
    /// `cf_clearance` to protect and the Client's default UA is used.
    async fn current_ua(&self) -> Option<String> {
        self.fs_ua.lock().await.clone()
    }

    /// Build a GET request builder, overriding the User-Agent with `ua` when
    /// present so the reqwest call matches the UA `cf_clearance` was minted
    /// under. Used for every direct (non-FlareSolverr) fetch.
    fn get_with_ua(&self, url: &str, ua: Option<&str>) -> reqwest::RequestBuilder {
        let mut req = self.http.get(url);
        if let Some(ua) = ua {
            req = req.header(header::USER_AGENT, ua);
        }
        req
    }

    /// Fetch a score page and extract everything the download pipeline needs
    /// from it: the candidate bundle URLs (salt lives in one of them), the
    /// page count, and the statically-embedded page-0 image URL.
    async fn fetch_score_page(&self, id: &str) -> anyhow::Result<ScorePage> {
        let url = format!("https://musescore.com/score/{id}");
        let html = self.fetch_html_challenged(&url, "page").await?;

        // None of these are fatal on their own anymore: a missing bundle just
        // means we lean on the fallback salt; a missing page-0 URL means we
        // resolve page 0 via jmuse like the rest; a missing page count
        // defaults to 1. Only a page we can't fetch at all aborts the job.
        let bundle_urls = extract_bundle_urls(&html);
        let pages_count = extract_pages_count(&html);
        let first_page_url = extract_first_page_url(&html);
        if bundle_urls.is_empty() {
            tracing::debug!(
                id,
                "musescore: no bundle URL on score page; relying on fallback salt"
            );
        }
        Ok(ScorePage {
            bundle_urls,
            pages_count,
            first_page_url,
        })
    }

    /// Extract the per-deploy `salt` MuseScore concatenates into the jmuse
    /// MD5 token. MuseScore ships several bundles per page and moves the
    /// token code between chunks across deploys, so — like dl-librescore —
    /// we walk *every* candidate bundle and return the salt from the first
    /// one that carries the `"…").substr(0, 4)` literal. Returns `None` when
    /// no bundle yields a salt; the caller then falls back to
    /// [`FALLBACK_SALT`]. The winning (bundle_url, salt) pair is cached; a
    /// later page whose bundle set still contains that URL reuses it without
    /// re-downloading.
    async fn prepare_algorithm(&self, bundle_urls: &[String], ua: Option<&str>) -> Option<String> {
        {
            let guard = self.cached.lock().await;
            if let Some(cached) = guard.as_ref() {
                if bundle_urls.iter().any(|u| u == &cached.bundle_url) {
                    return Some(cached.random_token.clone());
                }
            }
        }

        for bundle_url in bundle_urls {
            let bundle = match self
                .get_with_ua(bundle_url, ua)
                .send()
                .await
                .and_then(|r| r.error_for_status())
            {
                Ok(r) => match r.text().await {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::debug!(bundle_url = %bundle_url, error = %e, "musescore: bundle body read failed; trying next");
                        continue;
                    }
                },
                Err(e) => {
                    tracing::debug!(bundle_url = %bundle_url, error = %e, "musescore: bundle fetch failed; trying next");
                    continue;
                }
            };

            if let Some(salt) = find_random_token(&bundle) {
                let mut guard = self.cached.lock().await;
                *guard = Some(CachedAlgorithm {
                    bundle_url: bundle_url.clone(),
                    random_token: salt.clone(),
                });
                return Some(salt);
            }
        }
        None
    }

    /// Hit `/api/jmuse` for a single (id, type, index) tuple and return the
    /// resolved CDN URL.
    async fn jmuse_url(
        &self,
        token: &str,
        referer: &str,
        id: &str,
        media_type: &str,
        index: usize,
        ua: Option<&str>,
    ) -> anyhow::Result<String> {
        let url = format!("https://musescore.com/api/jmuse?id={id}&index={index}&type={media_type}");
        let http_resp = self
            .get_with_ua(&url, ua)
            .header(reqwest::header::AUTHORIZATION, token)
            .header(reqwest::header::REFERER, referer)
            .send()
            .await
            .context("musescore jmuse request")?;

        // Read the body before raising on non-2xx. MuseScore returns a JSON
        // explanation on 401/403/404 (bad token vs. Pro-gated page vs. index
        // out of range); `error_for_status()` would discard it, leaving a bare
        // status that can't tell those apart. A page index past the free
        // preview on a paid score 404s here — distinguishable only by the body.
        let status = http_resp.status();
        let body = http_resp
            .text()
            .await
            .context("musescore jmuse body")?;
        if !status.is_success() {
            let snippet: String = body.chars().take(400).collect();
            anyhow::bail!(
                "musescore jmuse non-success status={status} index={index} type={media_type} body={snippet:?}"
            );
        }
        let resp: JmuseResponse =
            serde_json::from_str(&body).context("musescore jmuse json")?;

        if resp.result != "success" {
            anyhow::bail!("musescore jmuse error: {:?}", resp.error);
        }
        let info = resp
            .info
            .ok_or_else(|| anyhow::anyhow!("musescore jmuse missing info for type={media_type}"))?;
        if info.url.is_empty() {
            anyhow::bail!("musescore jmuse returned empty url for type={media_type} index={index}");
        }
        Ok(info.url)
    }

    /// GET a CDN URL and return the body, bounded by `max_bytes`.
    async fn fetch_bytes(&self, url: &str, max_bytes: usize, ua: Option<&str>) -> anyhow::Result<Vec<u8>> {
        let resp = self
            .get_with_ua(url, ua)
            .send()
            .await
            .context("musescore cdn fetch")?
            .error_for_status()
            .context("musescore cdn status")?;
        if let Some(len) = resp.content_length() {
            anyhow::ensure!(
                (len as usize) <= max_bytes,
                "musescore asset too large ({len} > {max_bytes})"
            );
        }
        let mut buf = Vec::with_capacity(64 * 1024);
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if buf.len() + chunk.len() > max_bytes {
                anyhow::bail!("musescore asset exceeds {max_bytes} bytes during streaming");
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(buf)
    }

    /// Resolve a single page's CDN image URL through `/api/jmuse`. Tries the
    /// candidate salts in priority order (bundle-extracted first, then
    /// [`FALLBACK_SALT`]) until the server accepts one, and records the
    /// winner in `working_salt` so every subsequent page mints straight from
    /// it — no wasted jmuse round-trips on the hair-trigger-rate-limited
    /// endpoint. Returns the last error if every candidate is rejected.
    async fn resolve_page_url(
        &self,
        id: &str,
        referer: &str,
        index: usize,
        candidate_salts: &[String],
        working_salt: &mut Option<String>,
        ua: Option<&str>,
    ) -> anyhow::Result<String> {
        if let Some(salt) = working_salt.clone() {
            let token = mint_token(&salt, id, "img", index);
            return self.jmuse_url(&token, referer, id, "img", index, ua).await;
        }

        let mut last_err: Option<anyhow::Error> = None;
        for salt in candidate_salts {
            let token = mint_token(salt, id, "img", index);
            match self.jmuse_url(&token, referer, id, "img", index, ua).await {
                Ok(url) => {
                    *working_salt = Some(salt.clone());
                    return Ok(url);
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err
            .unwrap_or_else(|| anyhow::anyhow!("no salt candidates available for page {index}")))
    }
}

#[async_trait]
impl Source for Musescore {
    fn id(&self) -> &'static str {
        "musescore"
    }

    fn display_name(&self) -> &'static str {
        "MuseScore"
    }

    fn external_url(&self, id: &str) -> String {
        format!("https://musescore.com/score/{id}")
    }

    async fn search(
        &self,
        query: &str,
        filters: &SearchFilters,
        limit: usize,
    ) -> anyhow::Result<Vec<SearchResult>> {
        self.mark_activity();
        // MuseScore's /sheetmusic search page accepts an `instrument` slug
        // param. Slugs line up with our Instrument::slug() values for the
        // common cases; for ones MuseScore doesn't recognise the param is
        // silently ignored, leaving the bare text search.
        let url = match filters.instrument {
            Some(inst) => format!(
                "https://musescore.com/sheetmusic?text={}&instrument={}",
                urlencoding::encode(query),
                inst.slug()
            ),
            None => format!(
                "https://musescore.com/sheetmusic?text={}",
                urlencoding::encode(query)
            ),
        };
        let html = self.fetch_html_challenged(&url, "search").await?;

        // Two-tier extraction:
        //   1. SSR hydration JSON, when we can get it (cookie-replay path
        //      after the first FS call lands us back on the SSR shell).
        //   2. Post-React DOM scrape, when FlareSolverr handed back the
        //      fully-rendered page with the hydration `data-<hex>=`
        //      attribute already stripped by client-side cleanup.
        // We try JSON first because it carries richer metadata (pages
        // count, instrumentations, composer); DOM scrape is leaner but
        // sufficient for the must-have fields (id, title, href).
        let scores = match extract_search_scores(&html) {
            Some(s) => s,
            None => match extract_search_scores_from_dom(&html) {
                Some(s) => {
                    tracing::debug!(
                        count = s.len(),
                        "musescore: SSR hydration absent, used DOM fallback"
                    );
                    s
                }
                None => {
                    // Both extractors failed. Capture enough structural
                    // signal that the next iteration of the DOM scraper
                    // knows what to look for. score_link_count tells us
                    // whether the rendered cards are present at all;
                    // first_card_html dumps the surrounding markup of
                    // the earliest score-page link we found so we can
                    // refine selectors next round.
                    let looks_like_cf = looks_like_cf_challenge(&html);
                    let has_data_hash = scan_for_data_hash_attr(&html);
                    let (score_link_count, first_card_html) =
                        diagnose_score_links(&html);
                    let snippet_head = truncate_for_log(&html, 1000);
                    tracing::warn!(
                        bytes = html.len(),
                        looks_like_cf,
                        has_data_hash,
                        score_link_count,
                        first_card_html = %first_card_html,
                        snippet_head = %snippet_head,
                        "musescore search: neither JSON nor DOM extraction matched"
                    );
                    return Ok(Vec::new());
                }
            },
        };

        let mut results = Vec::with_capacity(scores.len().min(limit));
        for s in scores.into_iter().take(limit) {
            let title = strip_highlight_markers(&s.title);
            let mut metadata: Vec<MetadataBadge> = Vec::new();
            if let Some(p) = s.pages_count {
                if p > 0 {
                    metadata.push(MetadataBadge {
                        label: if p == 1 {
                            "1 page".to_string()
                        } else {
                            format!("{p} pages")
                        },
                        kind: BadgeKind::Pages,
                    });
                }
            }
            if let Some(parts) = s.parts_count {
                if parts > 1 {
                    metadata.push(MetadataBadge {
                        label: format!("{parts} parts"),
                        kind: BadgeKind::Generic,
                    });
                }
            }
            for inst in s.instrumentations {
                metadata.push(MetadataBadge {
                    label: inst,
                    kind: BadgeKind::Instrument,
                });
            }
            // Surface difficulty as a badge alongside the existing pills.
            // The filter wiring lands on top of this; the badge stays even
            // when no difficulty filter is active so users see the level.
            if let Some(level) = s.complexity {
                let label = match level {
                    1 => "Beginner",
                    2 => "Intermediate",
                    3 => "Advanced",
                    _ => "",
                };
                if !label.is_empty() {
                    metadata.push(MetadataBadge {
                        label: label.to_string(),
                        kind: BadgeKind::Difficulty,
                    });
                }
            }
            results.push(SearchResult {
                source: "musescore".to_string(),
                id: s.id.to_string(),
                title,
                description: s.composer_name,
                external_url: s
                    .href
                    .unwrap_or_else(|| format!("https://musescore.com/score/{}", s.id)),
                thumbnail_url: s.thumbnail_url,
                metadata,
                complexity: s.complexity,
                is_public_domain: s.is_public_domain,
                is_official: s.is_official,
            });
        }
        Ok(results)
    }

    async fn fetch_pdf_bytes(&self, id: &str, max_bytes: usize) -> anyhow::Result<Vec<u8>> {
        self.mark_activity();
        let page = self.fetch_score_page(id).await?;
        // UA the score page (and therefore the fresh cf_clearance cookie) was
        // just solved under. Replay it on the direct bundle/jmuse/CDN calls so
        // Cloudflare doesn't re-challenge them under a mismatched fingerprint.
        // Snapshot *after* fetch_score_page so we capture the UA of the solve
        // it may have triggered. None in direct-only (no-FlareSolverr) mode.
        let ua = self.current_ua().await;
        let ua_ref = ua.as_deref();

        // A missing page count means the page layout drifted out from under
        // both extractors. We assume 1 so single-page scores still work, but
        // warn loudly: for a multi-page score this silently truncates to page
        // 0, producing a valid-looking but incomplete PDF. The warning surfaces
        // that in logs / source health rather than failing silently.
        if page.pages_count.is_none() {
            tracing::warn!(
                id,
                "musescore: page count unreadable; assuming 1 page (a multi-page score would be truncated)"
            );
        }
        let pages_count = page.pages_count.unwrap_or(1).max(1);
        anyhow::ensure!(pages_count <= 200, "musescore score has implausible pages_count={pages_count}");
        let referer = self.external_url(id);

        // SVG scores would need a rasterizer we don't bundle (printpdf is
        // PNG-only). Detect from the page-0 preload URL and fail with a clear
        // message rather than a cryptic decode error deep inside assembly.
        if let Some(u) = &page.first_page_url {
            anyhow::ensure!(
                !is_svg_asset(u),
                "musescore score {id} serves SVG page images, which aren't supported yet"
            );
        }

        // Candidate salts in priority order: the one lifted from the live
        // bundle (if any), then dl-librescore's hardcoded fallback. Deduped so
        // a bundle that literally ships `9654,4e` isn't tried twice.
        let extracted = self.prepare_algorithm(&page.bundle_urls, ua_ref).await;
        let mut candidate_salts: Vec<String> = Vec::new();
        if let Some(s) = extracted {
            candidate_salts.push(s);
        }
        if !candidate_salts.iter().any(|s| s == FALLBACK_SALT) {
            candidate_salts.push(FALLBACK_SALT.to_string());
        }

        tracing::info!(
            id,
            pages_count,
            salt_candidates = candidate_salts.len(),
            static_page0 = page.first_page_url.is_some(),
            "musescore: resolving page images"
        );

        // Resolve a CDN image URL per page. Page 0's URL is embedded in the
        // score page itself (dl-librescore's trick), so a single-page score
        // needs no token at all. Remaining pages hit `/api/jmuse` *serially*
        // (its per-IP rate limit is hair-trigger), trying each candidate salt
        // until one is accepted and then reusing that salt.
        let mut working_salt: Option<String> = None;
        let mut png_urls: Vec<String> = Vec::with_capacity(pages_count);
        for index in 0..pages_count {
            if index == 0 {
                if let Some(u) = &page.first_page_url {
                    png_urls.push(u.clone());
                    continue;
                }
            }
            let url = self
                .resolve_page_url(id, &referer, index, &candidate_salts, &mut working_salt, ua_ref)
                .await
                .with_context(|| format!("resolving CDN url for page {index}"))?;
            anyhow::ensure!(
                !is_svg_asset(&url),
                "musescore score {id} serves SVG page images, which aren't supported yet"
            );
            png_urls.push(url);
        }

        // Download the page PNGs from the CDN with bounded concurrency. Unlike
        // `/api/jmuse`, the image CDN isn't rate-limited, so parallel fetches
        // are safe and cut multi-page latency. We fan out in chunks of
        // `PNG_FETCH_CONCURRENCY` (each chunk joined before the next starts);
        // page order is preserved because `join_all` returns results in input
        // order. Each PNG gets the full `max_bytes` budget; the aggregate cap
        // is enforced as the chunks land.
        let per_page_budget = max_bytes;
        let mut pngs: Vec<Vec<u8>> = Vec::with_capacity(pages_count);
        let mut running = 0usize;
        for chunk in png_urls.chunks(PNG_FETCH_CONCURRENCY) {
            let fetches = chunk
                .iter()
                .map(|url| self.fetch_bytes(url, per_page_budget, ua_ref));
            for bytes in futures_util::future::join_all(fetches).await {
                let bytes = bytes?;
                running = running.saturating_add(bytes.len());
                anyhow::ensure!(
                    running <= max_bytes,
                    "musescore PNGs aggregate exceeds {max_bytes} bytes"
                );
                pngs.push(bytes);
            }
        }

        let pdf_bytes = tokio::task::spawn_blocking(move || assemble_pdf(&pngs))
            .await
            .context("pdf-assemble task join")??;

        anyhow::ensure!(
            pdf_bytes.len() <= max_bytes,
            "musescore assembled PDF exceeds {max_bytes} bytes"
        );
        Ok(pdf_bytes)
    }
}

#[derive(Debug, Deserialize)]
struct JmuseResponse {
    result: String,
    #[serde(default)]
    error: Option<serde_json::Value>,
    #[serde(default)]
    info: Option<JmuseInfo>,
}

#[derive(Debug, Deserialize)]
struct JmuseInfo {
    #[serde(default)]
    url: String,
}

#[derive(Debug)]
struct ScoreMeta {
    pages_count: Option<usize>,
}

/// Everything the download pipeline lifts from a fetched score page. All
/// fields are best-effort: an empty `bundle_urls` falls back to
/// [`FALLBACK_SALT`], a `None` `first_page_url` resolves page 0 via jmuse
/// like any other page, and a `None` `pages_count` defaults to 1.
#[derive(Debug)]
struct ScorePage {
    /// Candidate app-bundle JS URLs, in document order, that may carry the
    /// jmuse salt literal.
    bundle_urls: Vec<String>,
    /// Rendered page count, when we could read it from the page.
    pages_count: Option<usize>,
    /// The statically-embedded page-0 image URL (`<link as="image">`), which
    /// needs no token — dl-librescore's first-page shortcut.
    first_page_url: Option<String>,
}

#[derive(Debug)]
struct SearchScore {
    id: u64,
    title: String,
    composer_name: Option<String>,
    href: Option<String>,
    thumbnail_url: Option<String>,
    /// Total rendered pages, when present in the hydration payload. Used
    /// for a "N pages" badge.
    pages_count: Option<usize>,
    /// Number of parts/voices; only emitted as a badge when > 1.
    parts_count: Option<usize>,
    /// Free-text instrumentation labels (e.g. "Piano", "Voice"). MuseScore
    /// usually returns 1–3 entries per score; we render each as its own
    /// pill so they stay short.
    instrumentations: Vec<String>,
    /// 1 = Beginner, 2 = Intermediate, 3 = Advanced. None when the field
    /// is absent or out-of-range (defensive against schema drift).
    complexity: Option<u8>,
    /// Per-score public-domain flag from the hydration JSON. None on the
    /// DOM-scrape fallback path (the JSON is no longer in the DOM).
    is_public_domain: Option<bool>,
    /// True for "official" publisher engravings, false for community
    /// uploads. None on the DOM-scrape fallback path.
    is_official: Option<bool>,
}

/// Heuristic for "this HTML is a Cloudflare interstitial, not the page we
/// asked for". Covers both the JS "Just a moment…" challenge and the hard
/// "Attention Required" block page. Cloudflare serves the challenge with a
/// 200 (not a 403) when a `cf_clearance` cookie has expired, so the
/// direct-clearance fast path needs this body check on top of the status
/// check to recognise a stale cookie and fall back to FlareSolverr.
fn looks_like_cf_challenge(html: &str) -> bool {
    html.contains("Just a moment") || html.contains("Attention Required")
}

/// Current Unix time in whole seconds. Clamps a pre-epoch clock to 0 rather
/// than panicking; only feeds the keep-warm idle heuristic, so a coarse
/// value is fine.
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Collect every candidate MuseScore app-bundle JS URL on the page, in
/// document order and deduped. Port of dl-librescore's suffix regex
/// (`file.ts`):
///   `/static/public/build/musescore.*?(?:_es6)?/20….js`
/// Deliberately looser than a single strict match: MuseScore ships several
/// bundles and relocates the token code between chunks across deploys, so the
/// caller ([`Musescore::prepare_algorithm`]) tries each until one yields the
/// salt literal, instead of betting on one filename shape.
fn extract_bundle_urls(html: &str) -> Vec<String> {
    let needle = "https://musescore.com/static/public/build/";
    let mut out: Vec<String> = Vec::new();
    let mut start = 0;
    while let Some(rel) = html[start..].find(needle) {
        let abs = start + rel;
        let tail = &html[abs..];
        // Terminate on any character that can't appear in the URL — the same
        // set the single-bundle scanner used, plus backslash (escaped JSON).
        let term = tail
            .find(|c: char| matches!(c, '"' | '\'' | ' ' | '<' | '>' | '\n' | '\r' | '\\'))
            .unwrap_or(tail.len());
        let candidate = &tail[..term];
        if is_bundle_candidate(candidate) && !out.iter().any(|u| u == candidate) {
            out.push(candidate.to_string());
        }
        start = abs + needle.len();
    }
    out
}

/// True for a `…/static/public/build/musescore…/20….js` URL — dl-librescore's
/// bundle shape. We don't insist on a numeric filename head (MuseScore has
/// shipped the token code in differently-named chunks), only that it's a
/// `musescore*` build bundle under a `20YYMM` directory.
fn is_bundle_candidate(url: &str) -> bool {
    let prefix = "https://musescore.com/static/public/build/";
    let Some(rest) = url.strip_prefix(prefix) else {
        return false;
    };
    url.ends_with(".js") && rest.starts_with("musescore") && rest.contains("/20")
}

/// Best-effort page count. Tries the SSR hydration JSON first (richest,
/// matches the search path), then dl-librescore's leaner regex
/// (`/pages(?:&quot;|"):(\d+)/`) as a fallback for when the JSON layout has
/// drifted but the raw field is still in the HTML.
fn extract_pages_count(html: &str) -> Option<usize> {
    if let Some(meta) = extract_score_meta(html) {
        if let Some(pc) = meta.pages_count {
            return Some(pc);
        }
    }
    find_pages_count_regex(html)
}

/// Scan for `pages":<n>` / `pages&quot;:<n>` (the field survives even when the
/// surrounding JSON is HTML-escaped inside a data attribute).
fn find_pages_count_regex(html: &str) -> Option<usize> {
    for needle in ["pages\":", "pages&quot;:"] {
        let mut start = 0;
        while let Some(off) = html[start..].find(needle) {
            let abs = start + off + needle.len();
            let digits: String = html[abs..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(n) = digits.parse::<usize>() {
                if n > 0 {
                    return Some(n);
                }
            }
            start = abs;
        }
    }
    None
}

/// The statically-embedded page-0 image URL — MuseScore preloads it via
/// `<link rel="preload" as="image" href="…">`, so page 0 needs no jmuse token
/// (dl-librescore's first-page shortcut). Falls back to the first
/// `<img src*="score_0">` if the preload link isn't present. The `@<size>`
/// suffix is stripped (matching dl-librescore's `.split("@")[0]`) so page 0
/// comes back at the same full resolution as the jmuse-resolved pages, not a
/// scaled preview variant. SVG-vs-PNG discrimination is left to
/// [`is_svg_asset`] at the call site.
fn extract_first_page_url(html: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    let raw = {
        let link_sel = Selector::parse(r#"link[as="image"]"#).ok();
        let img_sel = Selector::parse(r#"img[src*="score_0"]"#).ok();
        link_sel
            .and_then(|sel| {
                doc.select(&sel)
                    .find_map(|el| el.value().attr("href").map(str::to_string))
            })
            .or_else(|| {
                img_sel.and_then(|sel| {
                    doc.select(&sel)
                        .find_map(|el| el.value().attr("src").map(str::to_string))
                })
            })
            .filter(|s| !s.is_empty())?
    };
    // Drop the `@0` / `@2x` CDN size tag (and anything after it) so we fetch
    // the canonical page image.
    Some(raw.split('@').next().unwrap_or(&raw).to_string())
}

/// True if `url` points at an SVG asset. MuseScore serves either PNG or SVG
/// page renderings; `printpdf` only decodes PNG, so the caller uses this to
/// fail SVG scores with a clear message. Strips a `@`-suffix (CDN size tag)
/// and any query string before checking the extension.
fn is_svg_asset(url: &str) -> bool {
    let base = url.split(['?', '@']).next().unwrap_or(url);
    base.ends_with(".svg")
}

/// Extract the `salt` string MuseScore feeds into the per-page jmuse token
/// (`md5(id + type + index + salt).substr(0, 4)`). We find the string literal
/// that immediately precedes `).substr(0, 4)` in the bundle.
///
/// Port of: `script.match(/"([\W\w]{1,50})"\)\.substr\(0, *4\)/)?.[1]`
fn find_random_token(s: &str) -> Option<String> {
    let mut start = 0;
    while let Some(pos) = s[start..].find("\")") {
        let abs_close = start + pos;
        // Walk backward to find the opening quote (up to 50 chars away).
        let lookback = abs_close.saturating_sub(50);
        if let Some(open) = s[lookback..abs_close].rfind('"') {
            let open_abs = lookback + open;
            if open_abs + 1 < abs_close {
                // Check that `.substr(0, 4)` (with optional spaces) follows.
                let after = &s[abs_close + 2..];
                if substr_zero_four_follows(after) {
                    return Some(s[open_abs + 1..abs_close].to_string());
                }
            }
        }
        start = abs_close + 2;
    }
    None
}

fn substr_zero_four_follows(after: &str) -> bool {
    let trimmed = after.trim_start();
    let Some(rest) = trimmed.strip_prefix(".substr(") else {
        return false;
    };
    let rest = rest.trim_start();
    let Some(rest) = rest.strip_prefix("0,") else {
        return false;
    };
    let rest = rest.trim_start();
    rest.starts_with('4')
}

/// Locate and parse the SSR hydration JSON inside the score page or search
/// page. MuseScore wraps the entire JSON state in a `data-<sha256>` attribute
/// on a `<div class="js-<sha256>">` element. We don't validate the matching
/// hash pair — we just take the first long-hex data attribute on the page.
fn find_hydration_json(html: &str) -> Option<String> {
    // Find `data-<hex60+>="..."`.
    let mut start = 0;
    while let Some(off) = html[start..].find("data-") {
        let abs = start + off;
        let tail = &html[abs + 5..];
        let hex_end = tail
            .find(|c: char| !(c.is_ascii_hexdigit()))
            .unwrap_or(tail.len());
        if hex_end >= 60 {
            let after = &tail[hex_end..];
            if let Some(eq) = after.strip_prefix("=\"") {
                let close = eq.find('"')?;
                let raw = &eq[..close];
                return Some(html_unescape(raw));
            }
        }
        start = abs + 5;
    }
    None
}

/// Decode HTML entities — both named (`&eacute;`, `&ndash;`, …) and
/// numeric (`&#039;`, `&#x2014;`). Used in two places:
///   1. Unescaping the `data-<hash>="…"` attribute that wraps the
///      hydration JSON: turns `&quot;` etc. back into JSON syntax.
///   2. Unescaping individual JSON string values (`title`,
///      `composer_name`) which MuseScore stores in HTML-encoded form.
fn html_unescape(s: &str) -> String {
    html_escape::decode_html_entities(s).into_owned()
}

/// Bound an error-log snippet to `max` Unicode characters. Used when
/// quoting upstream response bodies in `bail!` messages so a 4 MB
/// Cloudflare challenge page doesn't flood the logs.
fn truncate_for_log(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// DOM-based fallback extractor for the case where FlareSolverr returned
/// the post-React-hydration DOM and MuseScore's client code already
/// stripped the SSR `data-<hex>=…` attribute. We find every `<a>` element
/// pointing at `/scores/<digits>`, dedupe by score id, and emit a leaner
/// `SearchScore` (no pages_count / instrumentations — those only existed
/// in the JSON). Order is document order; on MuseScore search results
/// pages the result cards come before "related" / "featured" sections,
/// so naïve doc-order + dedup is good enough.
fn extract_search_scores_from_dom(html: &str) -> Option<Vec<SearchScore>> {
    let doc = Html::parse_document(html);
    let link_sel = Selector::parse(r#"a[href*="/scores/"]"#).ok()?;
    let img_sel = Selector::parse("img").ok()?;

    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<SearchScore> = Vec::new();

    for el in doc.select(&link_sel) {
        let href = match el.value().attr("href") {
            Some(h) => h,
            None => continue,
        };
        let id = match parse_score_id_from_url(href) {
            Some(id) => id,
            None => continue,
        };
        if !seen.insert(id) {
            continue;
        }

        // Title heuristic: image alt is usually the cleanest signal (cards
        // typically have `<img alt="<title>" />`); otherwise the anchor's
        // concatenated text content — noisier (may include duration,
        // composer, "Free" badge) but always present.
        let img_alt = el
            .select(&img_sel)
            .next()
            .and_then(|img| img.value().attr("alt"))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        let anchor_text = el.text().collect::<String>().trim().to_string();
        let title_raw = img_alt.unwrap_or(anchor_text);
        if title_raw.is_empty() {
            continue;
        }

        let thumbnail_url = el
            .select(&img_sel)
            .next()
            .and_then(|img| img.value().attr("src"))
            .filter(|s| !s.is_empty())
            .map(String::from);

        out.push(SearchScore {
            id,
            title: html_unescape(&title_raw),
            // Composer / pages / parts / instrumentations would need
            // sibling-element heuristics we can't write without sample
            // HTML; leave them out and the result card just won't carry
            // metadata badges. The title + thumbnail + link to PDF is
            // still enough for the primary user action.
            composer_name: None,
            href: Some(href.to_string()),
            thumbnail_url,
            pages_count: None,
            parts_count: None,
            instrumentations: vec![],
            // Difficulty / PD / official flags live only in the hydration
            // JSON; the DOM scraper sees the post-React DOM where they've
            // been stripped. None is "unknown", which is the correct
            // signal for the filter layer.
            complexity: None,
            is_public_domain: None,
            is_official: None,
        });
    }

    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Parse the numeric score id out of any URL containing `/scores/<digits>`.
/// Handles both absolute (`https://musescore.com/user/.../scores/N`) and
/// path-relative (`/user/.../scores/N`) forms, plus trailing `/edit`,
/// `?query=…`, `#fragment` suffixes.
fn parse_score_id_from_url(url: &str) -> Option<u64> {
    let after = url.split("/scores/").nth(1)?;
    let id_str = after.split(['/', '?', '#']).next()?;
    id_str.parse().ok()
}

/// Diagnostic: count `<a href*="/scores/">` links and emit a HTML
/// snippet of the FIRST match's enclosing structure. When both
/// extractors return empty, this is what tells us whether the cards
/// are rendered at all and what their markup looks like.
fn diagnose_score_links(html: &str) -> (usize, String) {
    let doc = Html::parse_document(html);
    let Ok(sel) = Selector::parse(r#"a[href*="/scores/"]"#) else {
        return (0, String::new());
    };
    let mut count = 0usize;
    let mut first_html: Option<String> = None;
    for el in doc.select(&sel) {
        count += 1;
        if first_html.is_none() {
            // Pull the surrounding structure (parent or grandparent) so we
            // see card wrapper classes, not just the anchor itself.
            let outer = el
                .parent()
                .and_then(scraper::ElementRef::wrap)
                .map(|p| p.html())
                .unwrap_or_else(|| el.html());
            first_html = Some(truncate_for_log(&outer, 1200));
        }
    }
    (count, first_html.unwrap_or_default())
}

/// True iff the HTML contains a `data-<60+ hex chars>="…"` attribute —
/// MuseScore's SSR hydration container. Used in diagnostics so we can tell
/// "FS handed us post-React-hydration DOM with the attribute stripped"
/// (false) apart from "the attribute is here, our parser is buggy" (true).
fn scan_for_data_hash_attr(html: &str) -> bool {
    let mut start = 0;
    while let Some(off) = html[start..].find("data-") {
        let abs = start + off;
        let tail = &html[abs + 5..];
        let hex_end = tail
            .find(|c: char| !c.is_ascii_hexdigit())
            .unwrap_or(tail.len());
        if hex_end >= 60 && tail[hex_end..].starts_with("=\"") {
            return true;
        }
        start = abs + 5;
    }
    false
}

fn extract_score_meta(html: &str) -> Option<ScoreMeta> {
    let json = find_hydration_json(html)?;
    let v: serde_json::Value = serde_json::from_str(&json).ok()?;
    let pages_count = find_pages_count(&v);
    Some(ScoreMeta { pages_count })
}

fn find_pages_count(v: &serde_json::Value) -> Option<usize> {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(pc) = map.get("pages_count").and_then(|x| x.as_u64()) {
                if map.contains_key("id") && map.contains_key("title") {
                    return Some(pc as usize);
                }
            }
            for (_, child) in map {
                if let Some(found) = find_pages_count(child) {
                    return Some(found);
                }
            }
            None
        }
        serde_json::Value::Array(arr) => arr.iter().find_map(find_pages_count),
        _ => None,
    }
}

fn extract_search_scores(html: &str) -> Option<Vec<SearchScore>> {
    let json = find_hydration_json(html)?;
    let v: serde_json::Value = serde_json::from_str(&json).ok()?;
    let scores = find_scores_array(&v)?;
    let mut out = Vec::with_capacity(scores.len());
    for s in scores {
        let id = s.get("id").and_then(|x| x.as_u64())?;
        // MuseScore's hydration JSON stores user-facing text HTML-escaped
        // even inside JSON string values, so titles come through as
        // "Pr&eacute;lude…" and "Sonata &ndash; First Movement". Decode
        // here so the rest of the pipeline (caching, dedup, render) sees
        // the real text.
        let title = html_unescape(s.get("title").and_then(|x| x.as_str())?);
        let composer_name = s
            .get("composer_name")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(html_unescape);
        let href = s
            .get("href")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
        // Try several known thumbnail field shapes — MuseScore's hydration
        // schema has shifted over time. First hit wins; None falls back to
        // the placeholder in the template.
        let thumbnail_url = s
            .get("thumbnails")
            .and_then(|t| {
                t.get("original")
                    .or_else(|| t.get("large"))
                    .or_else(|| t.get("medium"))
                    .or_else(|| t.get("small"))
            })
            .and_then(|x| x.as_str())
            .or_else(|| s.get("share_pic_url").and_then(|x| x.as_str()))
            .or_else(|| s.get("share_pic").and_then(|x| x.as_str()))
            .or_else(|| s.get("cover_pic").and_then(|x| x.as_str()))
            .or_else(|| s.get("thumbnail_url").and_then(|x| x.as_str()))
            .filter(|s| !s.is_empty())
            .map(String::from);
        // MuseScore's hydration JSON exposes pages_count and (less reliably)
        // parts_count and an instrumentations array. We pull whatever's
        // present; missing keys just mean no badge for that score.
        let pages_count = s
            .get("pages_count")
            .and_then(|x| x.as_u64())
            .map(|n| n as usize);
        let parts_count = s
            .get("parts_count")
            .and_then(|x| x.as_u64())
            .map(|n| n as usize);
        let instrumentations = extract_instrumentations(s.get("instrumentations"));
        // Difficulty / PD / official flags carried by the per-score JSON.
        // `complexity` is bounded 1..=3 in MuseScore's schema; we drop
        // anything outside that range rather than render a "Difficulty: 7"
        // badge if their schema drifts. `is_public_domain` is encoded as
        // 0/1 in the JSON (not a bool); `is_official` is a real bool.
        let complexity = s
            .get("complexity")
            .and_then(|x| x.as_u64())
            .filter(|n| (1..=3).contains(n))
            .map(|n| n as u8);
        let is_public_domain = s
            .get("is_public_domain")
            .and_then(|x| x.as_u64())
            .map(|n| n != 0);
        let is_official = s
            .get("is_official")
            .and_then(|x| x.as_bool());
        out.push(SearchScore {
            id,
            title,
            composer_name,
            href,
            thumbnail_url,
            pages_count,
            parts_count,
            instrumentations,
            complexity,
            is_public_domain,
            is_official,
        });
    }
    Some(out)
}

fn find_scores_array(v: &serde_json::Value) -> Option<Vec<serde_json::Value>> {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(arr) = map.get("scores").and_then(|x| x.as_array()) {
                if arr
                    .first()
                    .and_then(|x| x.as_object())
                    .map(|o| o.contains_key("id"))
                    .unwrap_or(false)
                {
                    return Some(arr.clone());
                }
            }
            for (_, child) in map {
                if let Some(found) = find_scores_array(child) {
                    return Some(found);
                }
            }
            None
        }
        serde_json::Value::Array(arr) => arr.iter().find_map(find_scores_array),
        _ => None,
    }
}

/// MuseScore's `instrumentations` field has varied shape across deploys:
/// sometimes a list of strings, sometimes a list of `{name: "Piano"}`
/// objects, sometimes absent. Normalise to a Vec<String> of cleaned labels.
/// We cap at 2 entries to keep the badge row visually quiet on dense
/// orchestral works.
fn extract_instrumentations(v: Option<&serde_json::Value>) -> Vec<String> {
    let Some(arr) = v.and_then(|x| x.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in arr {
        let label = if let Some(s) = entry.as_str() {
            Some(s.to_string())
        } else if let Some(obj) = entry.as_object() {
            obj.get("name")
                .and_then(|x| x.as_str())
                .map(String::from)
                .or_else(|| obj.get("title").and_then(|x| x.as_str()).map(String::from))
        } else {
            None
        };
        if let Some(l) = label {
            let l = l.trim();
            if !l.is_empty() {
                out.push(l.to_string());
            }
        }
        if out.len() >= 2 {
            break;
        }
    }
    out
}

/// MuseScore wraps highlighted query matches in `[b]...[/b]` (BBCode-ish).
fn strip_highlight_markers(s: &str) -> String {
    s.replace("[b]", "").replace("[/b]", "")
}

/// Mint the 4-character jmuse auth token for a single page index: the first 4
/// hex chars of `md5(id + type + index + salt)`, lowercase. MuseScore's bundle
/// computes exactly this (confirmed against musescore-downloader / dl-librescore
/// / yt-dlp / amuse), so we compute it natively instead of running their
/// frequently-changing minified JS through a JS engine — faster, and immune to
/// bundle syntax churn that used to break token minting on every deploy.
fn mint_token(salt: &str, score_id: &str, media_type: &str, index: usize) -> String {
    use md5::{Digest, Md5};
    let input = format!("{score_id}{media_type}{index}{salt}");
    let digest = Md5::digest(input.as_bytes());
    // First 4 hex chars of the digest == first 2 bytes as lowercase hex.
    format!("{:02x}{:02x}", digest[0], digest[1])
}

/// Stitch a Vec of PNG byte buffers into a single PDF document, one page
/// per image. Page size is derived from the PNG dimensions at 96 DPI so the
/// page proportions match the rendered score.
fn assemble_pdf(pngs: &[Vec<u8>]) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(!pngs.is_empty(), "no PNG pages to assemble");
    let mut doc = PdfDocument::new("MuseScore Score");
    let mut pages = Vec::with_capacity(pngs.len());

    let mut warnings = Vec::new();
    for (i, png_bytes) in pngs.iter().enumerate() {
        let image = RawImage::decode_from_bytes(png_bytes, &mut warnings)
            .map_err(|e| anyhow::anyhow!("decoding PNG page {i}: {e}"))?;
        let width_px = image.width as f32;
        let height_px = image.height as f32;
        let dpi = 96.0_f32;
        let width_mm = (width_px / dpi) * 25.4;
        let height_mm = (height_px / dpi) * 25.4;
        let image_id = doc.add_image(&image);
        let contents = vec![Op::UseXobject {
            id: image_id,
            transform: XObjectTransform::default(),
        }];
        pages.push(PdfPage::new(Mm(width_mm), Mm(height_mm), contents));
    }

    let bytes = doc
        .with_pages(pages)
        .save(&PdfSaveOptions::default(), &mut Vec::new());
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_urls_collects_all_candidates() {
        // Two app bundles (numeric-head and named) plus a decoy under a
        // non-musescore build path. dl-librescore's shape accepts the first
        // two and we now try both for the salt; the decoy is rejected.
        let html = r#"
            <link rel="preload" as="script" href="https://musescore.com/static/public/build/musescore_es6/202605/2946.39bf11a4f5f177d8d4f4d5c31f8d973e.js">
            <link rel="preload" as="script" href="https://musescore.com/static/public/build/musescore_es6/202605/ms.8161d273ff40c7bcaf29ff0743fbc076.js">
            <link rel="preload" as="script" href="https://musescore.com/static/public/build/vendor/deadbeef.js">
        "#;
        let urls = extract_bundle_urls(html);
        assert_eq!(urls.len(), 2, "got {urls:?}");
        assert!(urls[0].ends_with("2946.39bf11a4f5f177d8d4f4d5c31f8d973e.js"));
        assert!(urls[1].ends_with("ms.8161d273ff40c7bcaf29ff0743fbc076.js"));

        // Individual predicate: `musescore*/20*/….js` accepted; a numeric head
        // is no longer required (that was the too-strict old rule).
        assert!(is_bundle_candidate(
            "https://musescore.com/static/public/build/musescore_es6/202605/ms.abc.js"
        ));
        assert!(!is_bundle_candidate(
            "https://musescore.com/static/public/build/vendor/deadbeef.js"
        ));
        // Not a .js
        assert!(!is_bundle_candidate(
            "https://musescore.com/static/public/build/musescore_es6/202605/app.css"
        ));
    }

    #[test]
    fn mint_token_native_md5() {
        // Token = first 4 hex chars of md5(score_id + type + index + salt),
        // lowercase. With all-empty score_id/type/salt the per-index input is
        // just the index digit, so we can pin against well-known MD5 vectors:
        //   md5("0") = cfcd208495d565ef66e7dff9f98764da
        //   md5("1") = c4ca4238a0b923820dcc509a6f75849b
        assert_eq!(mint_token("", "", "", 0), "cfcd");
        assert_eq!(mint_token("", "", "", 1), "c4ca");

        // The dl-librescore fallback salt contains a comma — make sure it
        // flows through the md5 input untouched (the token is just 4 lowercase
        // hex chars; the exact value isn't a known vector).
        let one = mint_token(FALLBACK_SALT, "123456", "img", 3);
        assert_eq!(one.len(), 4);
        assert!(one
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn svg_asset_detection() {
        assert!(is_svg_asset(
            "https://musescore.com/static/.../score_0.svg"
        ));
        assert!(is_svg_asset(
            "https://example.com/score_0.svg@0"
        ));
        assert!(!is_svg_asset(
            "https://musescore.com/static/.../score_0.png@0"
        ));
        assert!(!is_svg_asset("https://example.com/score_0.png?foo=bar"));
    }

    #[test]
    fn first_page_url_from_preload_link() {
        // Preferred: the preload <link as="image">, with the @<size> tag
        // stripped so page 0 matches the jmuse pages' resolution.
        let html = r#"<link rel="preload" href="https://musescore.com/static/x/score_0.png@0" as="image">"#;
        assert_eq!(
            extract_first_page_url(html).as_deref(),
            Some("https://musescore.com/static/x/score_0.png")
        );
        // Fallback: first <img src*="score_0"> when no preload link exists.
        // SVG has no @-tag, so it passes through unchanged.
        let html2 = r#"<div><img src="https://cdn/score_0.svg" alt="p1"></div>"#;
        assert_eq!(
            extract_first_page_url(html2).as_deref(),
            Some("https://cdn/score_0.svg")
        );
        // Neither present.
        assert_eq!(extract_first_page_url("<html></html>"), None);
    }

    #[test]
    fn pages_count_regex_fallback() {
        // Escaped-JSON form inside a data attribute.
        assert_eq!(find_pages_count_regex(r#"...&quot;pages&quot;:12,..."#), Some(12));
        // Plain-JSON form.
        assert_eq!(find_pages_count_regex(r#"{"pages":3,"foo":1}"#), Some(3));
        // Absent.
        assert_eq!(find_pages_count_regex("nothing here"), None);
    }

    #[test]
    fn strip_markers() {
        assert_eq!(
            strip_highlight_markers("[b]Fur[/b] [b]Elise[/b]"),
            "Fur Elise"
        );
    }

    #[test]
    fn cf_challenge_detection() {
        // Interactive JS challenge.
        assert!(looks_like_cf_challenge(
            "<title>Just a moment...</title><body>checking your browser</body>"
        ));
        // Hard block page.
        assert!(looks_like_cf_challenge(
            "<h1>Attention Required! | Cloudflare</h1>"
        ));
        // A real search page (has score links, no challenge markers) must
        // NOT be mistaken for a challenge, or the direct-clearance fast path
        // would needlessly fall back to FlareSolverr on every hit.
        assert!(!looks_like_cf_challenge(
            "<div class=\"score\"><a href=\"/score/12345\">Für Elise</a></div>"
        ));
    }

    // ----- Integration smoke test (Phase D) -----
    //
    // `#[ignore]` so it never runs as part of `cargo test`. Exercises the
    // whole MuseScore pipeline against the live site:
    //   search → score page → bundle fetch → salt extraction → native MD5 →
    //   /api/jmuse → per-page PNGs → printpdf assembly.
    //
    // Run manually with:
    //
    //     cargo test musescore_smoke -- --ignored --nocapture
    //
    // Failure modes guide where the pipeline is broken:
    //   * "musescore search HTTP …" — Phase B headers needed
    //   * "could not find musescore bundle URL …" — bundle URL extraction
    //   * "randomToken salt not found in musescore bundle" — find_random_token
    //   * "musescore jmuse error: …" — salt/token wrong, or MuseScore Pro content
    //   * "bytes don't start with %PDF-1." — printpdf re-encode regression
    //
    // Override the score id via MUSESCORE_SMOKE_QUERY / MUSESCORE_SMOKE_ID
    // env vars to pin to a known-stable score when MuseScore takes one
    // down. Default query "bach" should always return free public-domain
    // user uploads.
    #[tokio::test]
    #[ignore]
    async fn musescore_smoke_search_and_fetch_pdf() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();

        // The pipeline is direct-first (dl-librescore's method): from a
        // residential egress the direct fetch succeeds and no FlareSolverr is
        // needed, so this test runs unconditionally now. It only requires FS
        // when the host running it *is* Cloudflare-challenged (e.g. a data-
        // center IP). The GitHub Actions runner is such an IP and has no FS,
        // so there the direct fetch 403s and this test fails — the CI
        // `musescore-smoke` job runs it with `continue-on-error: true` so that
        // never blocks a deploy. Run it on the NAS (residential IP) to get a
        // real pass, with or without FLARESOLVERR_URL set.
        let m = Musescore::new().expect("Musescore::new");

        let query =
            std::env::var("MUSESCORE_SMOKE_QUERY").unwrap_or_else(|_| "bach".to_string());

        let results = m
            .search(&query, &SearchFilters::default(), 5)
            .await
            .expect("search must not error for a stable query");
        assert!(!results.is_empty(), "search returned 0 results for {query:?}");

        // If env-pinned, prefer that id. Otherwise try each result in
        // order until one's PDF resolves — guards against the top result
        // being MuseScore-Pro-only content.
        let candidates: Vec<String> = match std::env::var("MUSESCORE_SMOKE_ID").ok() {
            Some(id) => vec![id],
            None => results.iter().take(3).map(|r| r.id.clone()).collect(),
        };

        let mut last_err: Option<anyhow::Error> = None;
        for id in &candidates {
            match m.fetch_pdf_bytes(id, 25 * 1024 * 1024).await {
                Ok(bytes) => {
                    assert!(
                        bytes.starts_with(b"%PDF-1."),
                        "id={id} bytes don't start with %PDF-1.; got first 8 bytes: {:?}",
                        &bytes[..bytes.len().min(8)]
                    );
                    assert!(bytes.len() > 1024, "id={id} PDF suspiciously small: {} B", bytes.len());
                    eprintln!("OK: id={id} bytes={} (first 8 bytes look like a PDF)", bytes.len());
                    return;
                }
                Err(e) => {
                    eprintln!("id={id} fetch failed: {e:#}");
                    last_err = Some(e);
                }
            }
        }
        panic!(
            "all {} candidate score ids failed; last error: {:?}",
            candidates.len(),
            last_err.expect("at least one candidate"),
        );
    }
}
