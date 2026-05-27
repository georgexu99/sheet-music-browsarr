use anyhow::Context;
use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use sqlx::SqlitePool;
use time::format_description::well_known::Rfc3339;

pub async fn ensure_admin_user(
    pool: &SqlitePool,
    init_password: Option<&str>,
) -> anyhow::Result<()> {
    let existing: Option<(i64,)> = sqlx::query_as("SELECT id FROM admin_user WHERE id = 1")
        .fetch_optional(pool)
        .await?;

    if existing.is_some() {
        return Ok(());
    }

    let pwd = init_password.context(
        "MUSICARR_ADMIN_PASSWORD must be set on first run to seed the admin account",
    )?;

    let hash = hash_password(pwd)?;
    let now = time::OffsetDateTime::now_utc().format(&Rfc3339)?;
    sqlx::query("INSERT INTO admin_user (id, password_hash, updated_at) VALUES (1, ?, ?)")
        .bind(&hash)
        .bind(&now)
        .execute(pool)
        .await?;

    tracing::info!("seeded initial admin user");
    Ok(())
}

pub fn hash_password(pwd: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let argon = Argon2::default();
    let hash = argon
        .hash_password(pwd.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("argon2 hash: {e}"))?
        .to_string();
    Ok(hash)
}

pub async fn verify_admin(pool: &SqlitePool, pwd: &str) -> anyhow::Result<bool> {
    let row: Option<(String,)> = sqlx::query_as("SELECT password_hash FROM admin_user WHERE id = 1")
        .fetch_optional(pool)
        .await?;

    let Some((hash,)) = row else {
        return Ok(false);
    };

    let parsed = PasswordHash::new(&hash).map_err(|e| anyhow::anyhow!("parse hash: {e}"))?;
    let ok = Argon2::default()
        .verify_password(pwd.as_bytes(), &parsed)
        .is_ok();
    Ok(ok)
}
