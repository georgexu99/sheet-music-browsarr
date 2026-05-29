use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;
use tower_sessions::{Expiry, SessionManagerLayer};
use tower_sessions_sqlx_store::SqliteStore;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

mod audit;
mod auth;
mod cache;
mod config;
mod db;
mod i18n;
mod routes;
mod secrets;
mod sources;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer().json())
        .init();

    let cfg = config::Config::from_env()?;

    let pool = db::init_pool(&cfg.db_path).await?;
    db::run_migrations(&pool).await?;
    auth::ensure_admin_user(&pool, cfg.admin_password_init.as_deref()).await?;

    let session_store = SqliteStore::new(pool.clone());
    session_store.migrate().await?;

    let session_layer = SessionManagerLayer::new(session_store)
        .with_secure(cfg.secure_cookies)
        .with_expiry(Expiry::OnInactivity(time::Duration::days(30)));

    let imslp = sources::imslp::Imslp::new()?;
    let mutopia = sources::mutopia::Mutopia::new()?;
    let musescore = sources::musescore::Musescore::new()?;
    let sources: Vec<Arc<dyn sources::Source>> =
        vec![Arc::new(imslp), Arc::new(mutopia), Arc::new(musescore)];
    let source_ids: Vec<&'static str> = sources.iter().map(|s| s.id()).collect();
    let source_health = sources::health::new(&source_ids);
    let secrets = secrets::Secrets::new(&cfg.secret_key)?;
    let search_cache = cache::new_search_cache();
    // Durable L2 search cache is opt-in (matches the FLARESOLVERR_URL
    // pattern). When disabled the app uses moka L1 only — identical to the
    // pre-L2 behavior. When enabled it reuses the existing SQLite pool /
    // persistent volume, so cold MuseScore solves survive restarts.
    let search_cache_l2 = if cfg.persistent_search_cache {
        tracing::info!("persistent (L2) search cache enabled (SQLite-backed)");
        Some(cache::L2Cache::new(pool.clone()))
    } else {
        None
    };
    let thumbnail_cache = cache::new_thumbnail_cache();
    let thumbnail_bytes_cache = cache::new_thumbnail_bytes_cache();
    let state = routes::AppState {
        pool,
        sources,
        secrets,
        search_cache,
        search_cache_l2,
        thumbnail_cache,
        thumbnail_bytes_cache,
        library_path: cfg.library_path.clone(),
        source_health,
    };

    let app = Router::new()
        .merge(routes::public::router())
        .merge(routes::admin::router())
        .with_state(state)
        .layer(session_layer)
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http());

    let addr = SocketAddr::from(([0, 0, 0, 0], cfg.port));
    tracing::info!(%addr, "sheet-music-browsarr listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
