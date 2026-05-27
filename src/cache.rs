use std::sync::Arc;
use std::time::Duration;

use moka::future::Cache;

use crate::sources::{SearchResult, Source};

#[derive(Hash, Eq, PartialEq, Clone)]
pub struct CacheKey {
    pub source: &'static str,
    pub query: String,
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
    };
    if let Some(cached) = cache.get(&key).await {
        return Ok((*cached).clone());
    }
    let results = source.search(query, limit).await?;
    cache.insert(key, Arc::new(results.clone())).await;
    Ok(results)
}
