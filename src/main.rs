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
mod config;
mod db;
mod email;
mod i18n;
mod rate_limit;
mod routes;
mod secrets;
mod settings;
mod sources;
mod turnstile;

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
    let secrets = secrets::Secrets::new(&cfg.secret_key)?;
    let state = routes::AppState {
        pool,
        sources,
        secrets,
        library_path: cfg.library_path.clone(),
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
