use sqlx::{Row, SqlitePool};
use time::format_description::well_known::Rfc3339;

use crate::secrets::Secrets;

/// Known setting keys. Anything not in this list is rejected by the admin UI.
pub const SMTP_HOST: &str = "smtp_host";
pub const SMTP_PORT: &str = "smtp_port";
pub const SMTP_USER: &str = "smtp_user";
pub const SMTP_PASS: &str = "smtp_pass";
pub const SMTP_FROM: &str = "smtp_from";
pub const TURNSTILE_SITE_KEY: &str = "turnstile_site_key";
pub const TURNSTILE_SECRET_KEY: &str = "turnstile_secret_key";

pub const ENCRYPTED_KEYS: &[&str] = &[SMTP_PASS, TURNSTILE_SECRET_KEY];

pub fn is_encrypted(key: &str) -> bool {
    ENCRYPTED_KEYS.contains(&key)
}

pub async fn get(pool: &SqlitePool, key: &str) -> Option<String> {
    sqlx::query("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .map(|r| r.get::<String, _>("value"))
}

pub async fn get_secret(
    pool: &SqlitePool,
    secrets: &Secrets,
    key: &str,
) -> anyhow::Result<Option<String>> {
    let Some(stored) = get(pool, key).await else {
        return Ok(None);
    };
    if stored.is_empty() {
        return Ok(None);
    }
    secrets.decrypt(&stored).map(Some)
}

pub async fn set(
    pool: &SqlitePool,
    secrets: &Secrets,
    key: &str,
    value: &str,
) -> anyhow::Result<()> {
    let encrypted = is_encrypted(key);
    let stored = if encrypted {
        if value.is_empty() {
            // Empty input — clear the setting rather than encrypt empty string.
            String::new()
        } else {
            secrets.encrypt(value)?
        }
    } else {
        value.to_string()
    };
    let now = time::OffsetDateTime::now_utc().format(&Rfc3339)?;
    sqlx::query(
        "INSERT INTO settings (key, value, encrypted, updated_at) VALUES (?, ?, ?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, encrypted = excluded.encrypted, updated_at = excluded.updated_at",
    )
    .bind(key)
    .bind(&stored)
    .bind(if encrypted { 1 } else { 0 })
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}
