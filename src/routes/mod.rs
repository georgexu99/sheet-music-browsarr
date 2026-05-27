use sqlx::SqlitePool;

use crate::sources::imslp::Imslp;

pub mod admin;
pub mod public;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub imslp: Imslp,
    pub library_path: String,
}
