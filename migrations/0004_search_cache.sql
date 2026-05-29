-- Phase: L2 (durable) search-result cache.
--
-- The in-process moka cache (src/cache.rs) is the L1: fast, but lost on
-- every container restart. MuseScore search in particular costs a 5-45s
-- FlareSolverr Cloudflare solve on a cold query, so re-paying that after
-- each redeploy is painful. This table is the L2: a durable, optionally
-- cross-instance cache that lives on the same persistent SQLite volume as
-- the rest of the app state, so cached MuseScore solves survive restarts.
--
-- Read-through ordering: moka L1 -> this table L2 -> the real Source.
-- A miss at L2 populates both layers; an L2 hit re-warms L1.
--
-- `cache_key` is the serialized (source, query, limit, instrument) tuple
-- that uniquely identifies a cached search (mirrors src/cache.rs::CacheKey).
-- `payload` is the JSON-serialized Vec<SearchResult>. `expires_at` is an
-- RFC3339 UTC timestamp; rows past it are treated as absent and lazily
-- swept on access (no background job — this is a ~5-user instance).
CREATE TABLE search_cache (
  cache_key  TEXT PRIMARY KEY,
  source     TEXT NOT NULL,
  payload    TEXT NOT NULL,
  created_at TEXT NOT NULL,
  expires_at TEXT NOT NULL
);

-- Supports the lazy expiry sweep ("DELETE FROM search_cache WHERE expires_at < ?").
CREATE INDEX idx_search_cache_expires_at ON search_cache(expires_at);
