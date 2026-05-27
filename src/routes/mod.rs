use sqlx::SqlitePool;

pub mod admin;
pub mod public;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
}
