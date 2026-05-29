pub struct Config {
    pub port: u16,
    pub db_path: String,
    pub library_path: String,
    pub secret_key: String,
    pub admin_password_init: Option<String>,
    pub secure_cookies: bool,
    /// Opt-in durable (L2) search-result cache, persisted in the SQLite
    /// `search_cache` table. When true, cold MuseScore FlareSolverr solves
    /// survive container restarts (and can be shared across instances that
    /// mount the same DB) instead of being re-paid on every redeploy.
    ///
    /// Opt-in via `BROWSARR_PERSISTENT_SEARCH_CACHE=1` (matching the
    /// `FLARESOLVERR_URL` opt-in pattern). When unset/false the app uses the
    /// in-memory moka L1 cache only — identical to the pre-L2 behavior.
    pub persistent_search_cache: bool,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let port = std::env::var("BROWSARR_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8686);

        let db_path = std::env::var("BROWSARR_DB_PATH")
            .unwrap_or_else(|_| "sheet-music-browsarr.db".to_string());

        let library_path = std::env::var("BROWSARR_LIBRARY_PATH")
            .unwrap_or_else(|_| "./library".to_string());

        let secret_key = std::env::var("BROWSARR_SECRET_KEY")
            .map_err(|_| anyhow::anyhow!("BROWSARR_SECRET_KEY is required (>=16 chars, used to encrypt secrets at rest)"))?;

        let admin_password_init = std::env::var("BROWSARR_ADMIN_PASSWORD")
            .ok()
            .filter(|s| !s.is_empty());

        let secure_cookies = std::env::var("BROWSARR_SECURE_COOKIES")
            .ok()
            .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let persistent_search_cache = std::env::var("BROWSARR_PERSISTENT_SEARCH_CACHE")
            .ok()
            .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        Ok(Self {
            port,
            db_path,
            library_path,
            secret_key,
            admin_password_init,
            secure_cookies,
            persistent_search_cache,
        })
    }
}
