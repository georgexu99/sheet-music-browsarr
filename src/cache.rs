use std::sync::Arc;
use std::time::Duration;

use moka::future::Cache;

use crate::sources::{SearchResult, Source};

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
}

/// Cross-user search-result cache. Entries are `Arc<Vec<SearchResult>>` so
/// cache hits avoid cloning the inner Vec on the hot path (the caller
/// either uses the Arc directly or pays for an explicit clone).
pub type SearchCache = Cache<CacheKey, Arc<Vec<SearchResult>>>;

const CACHE_TTL_SECS: u64 = 60;
const CACHE_MAX_ENTRIES: u64 = 1000;

pub fn new_search_cache() -> SearchCache {
    Cache::builder()
        .max_capacity(CACHE_MAX_ENTRIES)
        .time_to_live(Duration::from_secs(CACHE_TTL_SECS))
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

/// Cache-aside wrapper around `Source::search`. Cache miss runs the real
/// search and inserts the result; cache hit returns the cached Vec
/// directly (callers clone as needed for ownership).
pub async fn cached_search(
    cache: &SearchCache,
    source: &Arc<dyn Source>,
    query: &str,
    limit: usize,
) -> anyhow::Result<Vec<SearchResult>> {
    let key = CacheKey {
        source: source.id(),
        query: query.to_string(),
        limit,
    };
    if let Some(cached) = cache.get(&key).await {
        return Ok((*cached).clone());
    }
    let results = source.search(query, limit).await?;
    cache.insert(key, Arc::new(results.clone())).await;
    Ok(results)
}
