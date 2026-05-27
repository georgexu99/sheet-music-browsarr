use std::path::PathBuf;

use askama::Template;
use askama_axum::IntoResponse;
use axum::extract::{Query, Request, State};
use axum::http::HeaderMap;
use axum::middleware::{self, Next};
use axum::response::{Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::Deserialize;
use sqlx::Row;
use time::format_description::well_known::Rfc3339;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tower_sessions::Session;

use crate::audit;
use crate::settings;
use crate::sources::health;

use super::AppState;

#[derive(Template)]
#[template(path = "admin/index.html")]
struct AdminIndex;

#[derive(Template)]
#[template(path = "admin/library.html")]
struct AdminLibrary {
    items: Vec<LibraryRow>,
    message: Option<String>,
}

struct LibraryRow {
    title: String,
    size_bytes: i64,
    added_at: String,
}

#[derive(Template)]
#[template(path = "admin/sources.html")]
struct AdminSources {
    health: Vec<HealthRow>,
    activity: Vec<ActivityRow>,
}

struct HealthRow {
    source_id: &'static str,
    status: &'static str,
    last_ok: Option<String>,
    last_error_at: Option<String>,
    last_error_msg: Option<String>,
    consecutive_fails: u32,
    consecutive_oks: u32,
    total_ok: u64,
    total_fail: u64,
}

struct ActivityRow {
    source: String,
    action: String,
    result: String,
    count: i64,
    last_ts: String,
}

#[derive(Template)]
#[template(path = "admin/search_log.html")]
struct AdminSearchLog {
    since: &'static str,
    recent: Vec<SearchLogRow>,
    top: Vec<TopQueryRow>,
}

struct SearchLogRow {
    query: String,
    query_truncated: bool,
    result: String,
    results_count: Option<i64>,
    variants_count: Option<i64>,
    ip: String,
    user_agent: String,
    ts: String,
}

struct TopQueryRow {
    query: String,
    query_truncated: bool,
    count: i64,
    distinct_ips: i64,
}

#[derive(Template)]
#[template(path = "admin/settings.html")]
struct AdminSettings {
    smtp_host: String,
    smtp_port: String,
    smtp_user: String,
    smtp_from: String,
    smtp_pass_set: bool,
    turnstile_site_key: String,
    turnstile_secret_set: bool,
    message: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin", get(index))
        .route("/admin/library", get(library_get))
        .route("/admin/library/add", post(library_add))
        .route("/admin/settings", get(settings_get).post(settings_post))
        .route("/admin/sources", get(sources_get))
        .route("/admin/search-log", get(search_log_get))
        .route_layer(middleware::from_fn(require_admin))
}

async fn require_admin(session: Session, req: Request, next: Next) -> Response {
    let signed_in: Option<bool> = session.get("admin").await.ok().flatten();
    if signed_in == Some(true) {
        next.run(req).await
    } else {
        Redirect::to("/login").into_response()
    }
}

async fn index() -> impl IntoResponse {
    AdminIndex
}

async fn library_get(State(state): State<AppState>) -> impl IntoResponse {
    let items = load_library(&state).await;
    AdminLibrary {
        items,
        message: None,
    }
}

async fn load_library(state: &AppState) -> Vec<LibraryRow> {
    match sqlx::query(
        "SELECT title, size_bytes, added_at FROM library_items ORDER BY added_at DESC LIMIT 200",
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|r| LibraryRow {
                title: r.get::<String, _>("title"),
                size_bytes: r.get::<i64, _>("size_bytes"),
                added_at: r.get::<String, _>("added_at"),
            })
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "load library failed");
            Vec::new()
        }
    }
}

#[derive(Deserialize)]
struct AddForm {
    source: String,
    item_id: String,
    title: String,
}

const ADMIN_MAX_PDF_BYTES: usize = 25 * 1024 * 1024;

async fn library_add(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<AddForm>,
) -> Response {
    let ip = audit::client_ip(&headers);
    let ua = audit::user_agent(&headers);

    let source_id = form.source.trim().to_string();
    let item_id = form.item_id.trim().to_string();
    let title = form.title.trim().to_string();
    if source_id.is_empty() || item_id.is_empty() || title.is_empty() {
        return render_library_with_message(&state, "Missing source, item_id, or title").await;
    }

    let source = match state.find_source(&source_id) {
        Some(s) => s,
        None => {
            return render_library_with_message(
                &state,
                &format!("Unknown source: {source_id}"),
            )
            .await;
        }
    };

    let bytes = match source.fetch_pdf_bytes(&item_id, ADMIN_MAX_PDF_BYTES).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, source = %source_id, id = %item_id, "library add: fetch failed");
            audit::record(
                &state.pool,
                &ip,
                ua.as_deref(),
                "library_add",
                Some(&format!("{source_id}/{item_id}")),
                "fetch_failed",
                None,
            )
            .await;
            return render_library_with_message(
                &state,
                &format!("Could not fetch PDF: {e}"),
            )
            .await;
        }
    };

    let safe = sanitize_filename(&title);
    let filename = format!("{safe}.pdf");
    let mut path = PathBuf::from(&state.library_path);
    if let Err(e) = fs::create_dir_all(&path).await {
        tracing::warn!(error = %e, path = %state.library_path, "library dir create");
    }
    path.push(&filename);

    let mut file = match fs::File::create(&path).await {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, ?path, "library add: file create");
            return render_library_with_message(&state, "Could not write file").await;
        }
    };
    if let Err(e) = file.write_all(&bytes).await {
        tracing::warn!(error = %e, "library add: file write");
        return render_library_with_message(&state, "Write error").await;
    }
    if let Err(e) = file.flush().await {
        tracing::warn!(error = %e, "library add: file flush");
    }
    let size = bytes.len() as i64;

    let now = match time::OffsetDateTime::now_utc().format(&Rfc3339) {
        Ok(t) => t,
        Err(_) => String::new(),
    };

    let path_str = path.to_string_lossy().to_string();
    let external_url = source.external_url(&item_id);

    let queue_id = sqlx::query(
        "INSERT INTO queue_items (title, source, source_url, state, local_path, size_bytes, progress, triggered_by_ip, created_at, updated_at) \
         VALUES (?, ?, ?, 'done', ?, ?, 1.0, NULL, ?, ?)",
    )
    .bind(&title)
    .bind(&source_id)
    .bind(&external_url)
    .bind(&path_str)
    .bind(size)
    .bind(&now)
    .bind(&now)
    .execute(&state.pool)
    .await
    .map(|r| r.last_insert_rowid());

    let queue_id = match queue_id {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "queue insert failed");
            return render_library_with_message(&state, "DB error (queue)").await;
        }
    };

    if let Err(e) = sqlx::query(
        "INSERT INTO library_items (queue_item_id, title, path, size_bytes, added_at) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(queue_id)
    .bind(&title)
    .bind(&path_str)
    .bind(size)
    .bind(&now)
    .execute(&state.pool)
    .await
    {
        tracing::warn!(error = %e, "library insert failed");
        return render_library_with_message(&state, "DB error (library)").await;
    }

    audit::record(
        &state.pool,
        &ip,
        ua.as_deref(),
        "library_add",
        Some(&format!("{source_id}/{item_id}")),
        "ok",
        Some(&format!(r#"{{"size":{size}}}"#)),
    )
    .await;

    render_library_with_message(&state, &format!("Added \"{title}\" ({size} bytes)")).await
}

async fn render_library_with_message(state: &AppState, message: &str) -> Response {
    let items = load_library(state).await;
    AdminLibrary {
        items,
        message: Some(message.to_string()),
    }
    .into_response()
}

fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '-' | '_' | ' ' => c,
            _ => '_',
        })
        .collect::<String>()
        .trim()
        .to_string()
}

async fn settings_get(State(state): State<AppState>) -> impl IntoResponse {
    render_settings(&state, None).await
}

#[derive(Deserialize)]
struct SettingsForm {
    #[serde(default)]
    smtp_host: String,
    #[serde(default)]
    smtp_port: String,
    #[serde(default)]
    smtp_user: String,
    #[serde(default)]
    smtp_pass: String,
    #[serde(default)]
    smtp_from: String,
    #[serde(default)]
    turnstile_site_key: String,
    #[serde(default)]
    turnstile_secret_key: String,
}

async fn settings_post(
    State(state): State<AppState>,
    Form(form): Form<SettingsForm>,
) -> Response {
    let pairs: Vec<(&str, &str, bool)> = vec![
        (settings::SMTP_HOST, form.smtp_host.trim(), true),
        (settings::SMTP_PORT, form.smtp_port.trim(), true),
        (settings::SMTP_USER, form.smtp_user.trim(), true),
        (settings::SMTP_FROM, form.smtp_from.trim(), true),
        (settings::TURNSTILE_SITE_KEY, form.turnstile_site_key.trim(), true),
        // Secret fields: only write if the user typed something (preserves existing on blank).
        (settings::SMTP_PASS, form.smtp_pass.as_str(), !form.smtp_pass.is_empty()),
        (
            settings::TURNSTILE_SECRET_KEY,
            form.turnstile_secret_key.as_str(),
            !form.turnstile_secret_key.is_empty(),
        ),
    ];

    for (key, value, should_write) in pairs {
        if !should_write {
            continue;
        }
        if let Err(e) = settings::set(&state.pool, &state.secrets, key, value).await {
            tracing::warn!(error = %e, key, "settings write failed");
            return render_settings(&state, Some(format!("Failed to save {key}: {e}"))).await.into_response();
        }
    }

    render_settings(&state, Some("Saved.".to_string())).await.into_response()
}

async fn render_settings(state: &AppState, message: Option<String>) -> AdminSettings {
    let smtp_host = settings::get(&state.pool, settings::SMTP_HOST).await.unwrap_or_default();
    let smtp_port = settings::get(&state.pool, settings::SMTP_PORT).await.unwrap_or_default();
    let smtp_user = settings::get(&state.pool, settings::SMTP_USER).await.unwrap_or_default();
    let smtp_from = settings::get(&state.pool, settings::SMTP_FROM).await.unwrap_or_default();
    let smtp_pass_set = settings::get(&state.pool, settings::SMTP_PASS)
        .await
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let turnstile_site_key = settings::get(&state.pool, settings::TURNSTILE_SITE_KEY)
        .await
        .unwrap_or_default();
    let turnstile_secret_set = settings::get(&state.pool, settings::TURNSTILE_SECRET_KEY)
        .await
        .map(|v| !v.is_empty())
        .unwrap_or(false);

    AdminSettings {
        smtp_host,
        smtp_port,
        smtp_user,
        smtp_from,
        smtp_pass_set,
        turnstile_site_key,
        turnstile_secret_set,
        message,
    }
}

async fn sources_get(State(state): State<AppState>) -> impl IntoResponse {
    let snapshot = health::snapshot(&state.source_health);
    let health: Vec<HealthRow> = snapshot
        .into_iter()
        .map(|(id, h)| HealthRow {
            source_id: id,
            status: if h.is_degraded() {
                "degraded"
            } else if h.last_ok.is_some() {
                "healthy"
            } else {
                "unknown"
            },
            last_ok: h.last_ok.and_then(|t| t.format(&Rfc3339).ok()),
            last_error_at: h.last_error_at.and_then(|t| t.format(&Rfc3339).ok()),
            last_error_msg: h.last_error_msg,
            consecutive_fails: h.consecutive_fails,
            consecutive_oks: h.consecutive_oks,
            total_ok: h.total_ok,
            total_fail: h.total_fail,
        })
        .collect();

    let activity = load_recent_activity(&state).await;
    AdminSources { health, activity }
}

/// Audit-log-backed per-source PDF + library_add history. The Plan agent's
/// recommendation: don't persist `SourceHealth` to SQLite, but DO surface
/// the audit log per-source so admins see "MuseScore was failing all
/// night while I was asleep." Search rows are aggregated across sources
/// so they're excluded here; live search status lives in the in-memory
/// `health` snapshot above.
async fn load_recent_activity(state: &AppState) -> Vec<ActivityRow> {
    let result = sqlx::query(
        r#"
        SELECT
            substr(target, 1, instr(target, '/') - 1) AS source,
            action,
            result,
            COUNT(*) AS cnt,
            MAX(ts) AS last_ts
        FROM audit_log
        WHERE action IN ('pdf', 'library_add')
          AND target LIKE '%/%'
        GROUP BY source, action, result
        ORDER BY last_ts DESC
        LIMIT 100
        "#,
    )
    .fetch_all(&state.pool)
    .await;

    match result {
        Ok(rows) => rows
            .into_iter()
            .map(|r| ActivityRow {
                source: r.get::<String, _>("source"),
                action: r.get::<String, _>("action"),
                result: r.get::<String, _>("result"),
                count: r.get::<i64, _>("cnt"),
                last_ts: r.get::<String, _>("last_ts"),
            })
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "load sources activity failed");
            Vec::new()
        }
    }
}

#[derive(Deserialize)]
struct SearchLogParams {
    #[serde(default)]
    since: Option<String>,
}

#[derive(Deserialize)]
struct SearchMeta {
    #[serde(default)]
    results: Option<i64>,
    #[serde(default)]
    variants: Option<i64>,
}

const QUERY_DISPLAY_MAX: usize = 80;

fn truncate_query(s: &str) -> (String, bool) {
    if s.chars().count() > QUERY_DISPLAY_MAX {
        let cut: String = s.chars().take(QUERY_DISPLAY_MAX).collect();
        (format!("{cut}…"), true)
    } else {
        (s.to_string(), false)
    }
}

/// Resolve the `?since=` window into (label, cutoff_ts_rfc3339).
/// Accepts "24h", "7d", "30d"; defaults to 7d.
fn resolve_since(raw: Option<&str>) -> (&'static str, String) {
    let label: &'static str = match raw.unwrap_or("7d") {
        "24h" => "24h",
        "30d" => "30d",
        _ => "7d",
    };
    let dur = match label {
        "24h" => time::Duration::hours(24),
        "30d" => time::Duration::days(30),
        _ => time::Duration::days(7),
    };
    let cutoff = time::OffsetDateTime::now_utc() - dur;
    let cutoff_s = cutoff.format(&Rfc3339).unwrap_or_default();
    (label, cutoff_s)
}

async fn search_log_get(
    State(state): State<AppState>,
    Query(params): Query<SearchLogParams>,
) -> impl IntoResponse {
    let (since_label, cutoff_ts) = resolve_since(params.since.as_deref());

    // Recent list: last 200 search rows in the window, newest first.
    let recent_rows = sqlx::query(
        r#"
        SELECT ts, ip, COALESCE(user_agent, '') AS user_agent, target, result, meta
        FROM audit_log
        WHERE action = 'search'
          AND ts >= ?
        ORDER BY ts DESC
        LIMIT 200
        "#,
    )
    .bind(&cutoff_ts)
    .fetch_all(&state.pool)
    .await;

    let recent: Vec<SearchLogRow> = match recent_rows {
        Ok(rows) => rows
            .into_iter()
            .map(|r| {
                let target: Option<String> = r.try_get::<Option<String>, _>("target").ok().flatten();
                let raw_q = target.unwrap_or_default();
                let (query, query_truncated) = truncate_query(&raw_q);
                let meta_str: Option<String> = r.try_get::<Option<String>, _>("meta").ok().flatten();
                let (results_count, variants_count) = match meta_str.as_deref() {
                    Some(s) if !s.is_empty() => match serde_json::from_str::<SearchMeta>(s) {
                        Ok(m) => (m.results, m.variants),
                        Err(_) => (None, None),
                    },
                    _ => (None, None),
                };
                SearchLogRow {
                    query,
                    query_truncated,
                    result: r.get::<String, _>("result"),
                    results_count,
                    variants_count,
                    ip: r.get::<String, _>("ip"),
                    user_agent: r.get::<String, _>("user_agent"),
                    ts: r.get::<String, _>("ts"),
                }
            })
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "load search log recent failed");
            Vec::new()
        }
    };

    // Top queries: aggregate by target text in the window.
    let top_rows = sqlx::query(
        r#"
        SELECT
            target AS query,
            COUNT(*) AS cnt,
            COUNT(DISTINCT ip) AS distinct_ips
        FROM audit_log
        WHERE action = 'search'
          AND result = 'ok'
          AND target IS NOT NULL
          AND ts >= ?
        GROUP BY target
        ORDER BY cnt DESC, query ASC
        LIMIT 50
        "#,
    )
    .bind(&cutoff_ts)
    .fetch_all(&state.pool)
    .await;

    let top: Vec<TopQueryRow> = match top_rows {
        Ok(rows) => rows
            .into_iter()
            .map(|r| {
                let raw_q: String = r.try_get::<Option<String>, _>("query").ok().flatten().unwrap_or_default();
                let (query, query_truncated) = truncate_query(&raw_q);
                TopQueryRow {
                    query,
                    query_truncated,
                    count: r.get::<i64, _>("cnt"),
                    distinct_ips: r.get::<i64, _>("distinct_ips"),
                }
            })
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "load search log top failed");
            Vec::new()
        }
    };

    AdminSearchLog {
        since: since_label,
        recent,
        top,
    }
}
