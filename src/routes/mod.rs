use std::sync::Arc;

use sqlx::SqlitePool;

use crate::cache::{SearchCache, ThumbnailBytesCache, ThumbnailCache};
use crate::secrets::Secrets;
use crate::sources::health::HealthMap;
use crate::sources::Source;

pub mod admin;
pub mod public;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub sources: Vec<Arc<dyn Source>>,
    pub secrets: Secrets,
    pub search_cache: SearchCache,
    /// Resolved-thumbnail URL cache for sources that need a lazy lookup
    /// (currently just IMSLP). 24h TTL; see `src/cache.rs`.
    pub thumbnail_cache: ThumbnailCache,
    /// Server-rendered thumbnail bytes cache for sources that generate
    /// inline PNGs (currently just Mutopia via pdftoppm). 24h TTL,
    /// 500-entry cap; see `src/cache.rs`.
    pub thumbnail_bytes_cache: ThumbnailBytesCache,
    pub library_path: String,
    /// In-memory per-source liveness (Phase G.0). Reset on container
    /// restart; durable history lives in `audit_log`. See
    /// `src/sources/health.rs` and `/admin/sources`.
    pub source_health: HealthMap,
}

impl AppState {
    pub fn find_source(&self, id: &str) -> Option<Arc<dyn Source>> {
        self.sources.iter().find(|s| s.id() == id).cloned()
    }
}
