use std::path::PathBuf;

use askama::Template;
use askama_axum::IntoResponse;
use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::middleware::{self, Next};
use axum::response::{Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use sqlx::Row;
use time::format_description::well_known::Rfc3339;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tower_sessions::Session;

use crate::audit;
use crate::settings;

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
    imslp_id: String,
    title: String,
}

async fn library_add(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<AddForm>,
) -> Response {
    let ip = audit::client_ip(&headers);
    let ua = audit::user_agent(&headers);

    let imslp_id = form.imslp_id.trim().to_string();
    let title = form.title.trim().to_string();
    if imslp_id.is_empty() || title.is_empty() {
        return render_library_with_message(&state, "Missing imslp_id or title").await;
    }

    let url = match state.imslp.fetch_pdf_url(&imslp_id).await {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(error = %e, id = %imslp_id, "library add: no pdf url");
            audit::record(
                &state.pool,
                &ip,
                ua.as_deref(),
                "library_add",
                Some(&imslp_id),
                "no_pdf_link",
                None,
            )
            .await;
            return render_library_with_message(&state, "Could not find a PDF link on IMSLP").await;
        }
    };

    let safe = sanitize_filename(&title);
    let filename = format!("{safe}.pdf");
    let mut path = PathBuf::from(&state.library_path);
    if let Err(e) = fs::create_dir_all(&path).await {
        tracing::warn!(error = %e, path = %state.library_path, "library dir create");
    }
    path.push(&filename);

    let resp = match state.imslp.http().get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "library add: upstream fetch failed");
            return render_library_with_message(&state, "Upstream fetch failed").await;
        }
    };
    if !resp.status().is_success() {
        tracing::warn!(status = %resp.status(), "library add: upstream not ok");
        return render_library_with_message(&state, "Upstream returned error").await;
    }
    let ct = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    if !ct.starts_with("application/pdf") {
        tracing::warn!(content_type = %ct, url = %url, "library add: upstream is not a PDF (likely IMSLP disclaimer)");
        return render_library_with_message(
            &state,
            "IMSLP returned a disclaimer page instead of a PDF. The pre-seeded accept cookies didn't match — try a different IMSLP page id, or open the page on imslp.org once to confirm a PDF is actually available.",
        )
        .await;
    }

    let mut size: i64 = 0;
    let mut file = match fs::File::create(&path).await {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, ?path, "library add: file create");
            return render_library_with_message(&state, "Could not write file").await;
        }
    };
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                if let Err(e) = file.write_all(&bytes).await {
                    tracing::warn!(error = %e, "library add: file write");
                    return render_library_with_message(&state, "Write error").await;
                }
                size += bytes.len() as i64;
            }
            Err(e) => {
                tracing::warn!(error = %e, "library add: chunk error");
                return render_library_with_message(&state, "Stream error").await;
            }
        }
    }
    if let Err(e) = file.flush().await {
        tracing::warn!(error = %e, "library add: file flush");
    }

    let now = match time::OffsetDateTime::now_utc().format(&Rfc3339) {
        Ok(t) => t,
        Err(_) => String::new(),
    };

    let path_str = path.to_string_lossy().to_string();

    let queue_id = sqlx::query(
        "INSERT INTO queue_items (title, source, source_url, state, local_path, size_bytes, progress, triggered_by_ip, created_at, updated_at) \
         VALUES (?, 'imslp', ?, 'done', ?, ?, 1.0, NULL, ?, ?)",
    )
    .bind(&title)
    .bind(&url)
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
        Some(&imslp_id),
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
