pub struct Config {
    pub port: u16,
    pub db_path: String,
    pub library_path: String,
    pub admin_password_init: Option<String>,
    pub secure_cookies: bool,
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

        let admin_password_init = std::env::var("BROWSARR_ADMIN_PASSWORD")
            .ok()
            .filter(|s| !s.is_empty());

        let secure_cookies = std::env::var("BROWSARR_SECURE_COOKIES")
            .ok()
            .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        Ok(Self {
            port,
            db_path,
            library_path,
            admin_password_init,
            secure_cookies,
        })
    }
}
