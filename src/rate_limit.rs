use sqlx::SqlitePool;
use time::format_description::well_known::Iso8601;

/// Default per-day quotas (Phase 2 hardcoded; settings page can override later).
pub const EMAIL_PER_IP_PER_DAY: i64 = 10;
pub const EMAIL_PER_RECIPIENT_PER_DAY: i64 = 5;
pub const EMAIL_GLOBAL_PER_DAY: i64 = 200;

/// Atomically check a daily quota and, if under the limit, increment it.
/// Returns true if the action is allowed; false if rate-limited.
pub async fn check_and_increment(
    pool: &SqlitePool,
    bucket: &str,
    limit: i64,
) -> anyhow::Result<bool> {
    let day = today_utc()?;

    let mut tx = pool.begin().await?;

    let current: Option<i64> = sqlx::query_scalar(
        "SELECT count FROM rate_buckets WHERE bucket_key = ? AND day = ?",
    )
    .bind(bucket)
    .bind(&day)
    .fetch_optional(&mut *tx)
    .await?;

    if current.unwrap_or(0) >= limit {
        return Ok(false);
    }

    sqlx::query(
        "INSERT INTO rate_buckets (bucket_key, day, count) VALUES (?, ?, 1) \
         ON CONFLICT(bucket_key, day) DO UPDATE SET count = count + 1",
    )
    .bind(bucket)
    .bind(&day)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(true)
}

fn today_utc() -> anyhow::Result<String> {
    // ISO 8601 date, e.g. "2026-05-27".
    let now = time::OffsetDateTime::now_utc().date();
    Ok(now.format(&Iso8601::DATE)?)
}
