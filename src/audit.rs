use axum::http::HeaderMap;
use sqlx::SqlitePool;
use time::format_description::well_known::Rfc3339;

/// Best-effort audit log insert. Failures are logged but don't break the
/// request that triggered them.
pub async fn record(
    pool: &SqlitePool,
    ip: &str,
    user_agent: Option<&str>,
    action: &str,
    target: Option<&str>,
    result: &str,
    meta: Option<&str>,
) {
    let now = match time::OffsetDateTime::now_utc().format(&Rfc3339) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "audit ts format");
            return;
        }
    };

    let res = sqlx::query(
        "INSERT INTO audit_log (ts, ip, user_agent, action, target, result, meta) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&now)
    .bind(ip)
    .bind(user_agent)
    .bind(action)
    .bind(target)
    .bind(result)
    .bind(meta)
    .execute(pool)
    .await;

    if let Err(e) = res {
        tracing::warn!(error = %e, "audit insert failed");
    }
}

/// Pull the originating client IP from request headers, preferring
/// `cf-connecting-ip` (set by Cloudflare tunnel), then `x-forwarded-for`,
/// falling back to a literal `"unknown"`.
pub fn client_ip(headers: &HeaderMap) -> String {
    if let Some(v) = headers.get("cf-connecting-ip").and_then(|h| h.to_str().ok()) {
        return v.trim().to_string();
    }
    if let Some(v) = headers.get("x-forwarded-for").and_then(|h| h.to_str().ok()) {
        return v.split(',').next().unwrap_or(v).trim().to_string();
    }
    "unknown".to_string()
}

pub fn user_agent(headers: &HeaderMap) -> Option<String> {
    headers
        .get("user-agent")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string())
}
