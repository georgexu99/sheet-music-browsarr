use std::sync::Arc;

use sqlx::SqlitePool;

use crate::cache::SearchCache;
use crate::secrets::Secrets;
use crate::sources::Source;

pub mod admin;
pub mod public;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub sources: Vec<Arc<dyn Source>>,
    pub secrets: Secrets,
    pub search_cache: SearchCache,
    pub library_path: String,
}

impl AppState {
    pub fn find_source(&self, id: &str) -> Option<Arc<dyn Source>> {
        self.sources.iter().find(|s| s.id() == id).cloned()
    }
}
