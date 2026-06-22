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

    // Reclaim any FlareSolverr sessions stranded by a previous instance
    // before the sources spin up their own. A redeploy restarts this
    // container but not FlareSolverr, so the prior instance's
    // `musescore-*` / `ultimateguitar` sessions (and their Chromium) leak;
    // worse, the new instance's `sessions.create` then collides with the
    // stale same-named session, fails, and silently degrades to sessionless
    // mode — which spawns even more browsers. Purging first makes the
    // subsequent pre-warm creates clean. No-op when FLARESOLVERR_URL is unset.
    if let Some(fs) = sources::flaresolverr::FlareSolverr::from_env()? {
        let reaped = fs.purge_all_sessions().await;
        tracing::info!(reaped, "flaresolverr: startup session purge complete");
    }

    let imslp = sources::imslp::Imslp::new()?;
    let mutopia = sources::mutopia::Mutopia::new()?;
    // MuseScore is held as a concrete Arc<Musescore> (not erased to
    // Arc<dyn Source>) long enough to kick off two background tasks that
    // need the concrete type: cookie keep-warm (so user requests stay on
    // the fast direct-replay path) and FS session-pool refresh (nightly
    // recycle of the long-lived Chromium contexts). After that it's
    // cloned into the type-erased registry like every other source.
    // Both spawns no-op when FLARESOLVERR_URL is unset.
    let musescore = Arc::new(sources::musescore::Musescore::new()?);
    Arc::clone(&musescore).spawn_warm_tasks();
    Arc::clone(&musescore).spawn_session_refresh();
    let ultimate_guitar = sources::ultimate_guitar::UltimateGuitar::new()?;
    let sources: Vec<Arc<dyn sources::Source>> = vec![
        Arc::new(imslp),
        Arc::new(mutopia),
        musescore,
        Arc::new(ultimate_guitar),
    ];
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
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // Drained on SIGTERM (Docker stop / Portainer redeploy) or Ctrl-C.
    // Destroy our FlareSolverr sessions so we don't strand their Chromium
    // for the next instance to inherit. Best-effort, time-boxed so a wedged
    // FS can't hold the process open past the container's stop grace period.
    if let Some(fs) = sources::flaresolverr::FlareSolverr::from_env()? {
        match tokio::time::timeout(std::time::Duration::from_secs(10), fs.purge_all_sessions())
            .await
        {
            Ok(reaped) => tracing::info!(reaped, "flaresolverr: shutdown session purge complete"),
            Err(_) => tracing::warn!("flaresolverr: shutdown session purge timed out"),
        }
    }
    Ok(())
}

/// Resolves when the process should begin a graceful shutdown: SIGTERM
/// (how Docker/Portainer stop a container) or Ctrl-C for local runs.
async fn shutdown_signal() {
    use tokio::signal;

    let ctrl_c = async {
        if let Err(e) = signal::ctrl_c().await {
            tracing::error!(error = %e, "failed to install Ctrl-C handler");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => tracing::error!(error = %e, "failed to install SIGTERM handler"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutdown signal received; draining connections");
}
