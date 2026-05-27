use std::collections::HashMap;

use askama::Template;
use askama_axum::IntoResponse;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use sqlx::SqlitePool;
use tower_sessions::Session;

use crate::audit;
use crate::auth;
use crate::email::{self as email_mod, SmtpConfig};
use crate::rate_limit;
use crate::secrets::Secrets;
use crate::settings;
use crate::sources::imslp::Imslp;
use crate::sources::SearchResult;
use crate::turnstile;

use super::AppState;

const MAX_PDF_BYTES: usize = 10 * 1024 * 1024;

#[derive(Template)]
#[template(path = "search.html")]
struct SearchPage {
    query: String,
    results: Vec<SearchResult>,
    turnstile_site_key: Option<String>,
    message: Option<String>,
}

#[derive(Template)]
#[template(path = "search_results.html")]
struct ResultsPartial {
    query: String,
    results: Vec<SearchResult>,
    turnstile_site_key: Option<String>,
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    error: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(home))
        .route("/healthz", get(healthz))
        .route("/search", get(search))
        .route("/pdf/imslp/:id", get(pdf_imslp))
        .route("/email", post(email_handler))
        .route("/login", get(login_page).post(login_submit))
        .route("/logout", post(logout))
}

async fn home(State(state): State<AppState>) -> impl IntoResponse {
    let site_key = settings::get(&state.pool, settings::TURNSTILE_SITE_KEY)
        .await
        .filter(|s| !s.is_empty());
    SearchPage {
        query: String::new(),
        results: Vec::new(),
        turnstile_site_key: site_key,
        message: None,
    }
}

async fn healthz() -> &'static str {
    "ok"
}

async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let query = params
        .get("q")
        .cloned()
        .unwrap_or_default()
        .trim()
        .to_string();
    let is_htmx = headers.get("hx-request").is_some();
    let ip = audit::client_ip(&headers);
    let ua = audit::user_agent(&headers);

    let site_key = settings::get(&state.pool, settings::TURNSTILE_SITE_KEY)
        .await
        .filter(|s| !s.is_empty());

    if query.is_empty() {
        audit::record(
            &state.pool,
            &ip,
            ua.as_deref(),
            "search",
            None,
            "empty_query",
            None,
        )
        .await;
        return render_results(is_htmx, &query, Vec::new(), site_key).into_response();
    }

    let results = match state.imslp.search(&query, 20).await {
        Ok(r) => {
            audit::record(
                &state.pool,
                &ip,
                ua.as_deref(),
                "search",
                Some(&query),
                "ok",
                Some(&format!(r#"{{"results":{}}}"#, r.len())),
            )
            .await;
            r
        }
        Err(e) => {
            tracing::warn!(error = %e, query = %query, "imslp search failed");
            audit::record(
                &state.pool,
                &ip,
                ua.as_deref(),
                "search",
                Some(&query),
                "upstream_error",
                None,
            )
            .await;
            Vec::new()
        }
    };

    render_results(is_htmx, &query, results, site_key).into_response()
}

fn render_results(
    is_htmx: bool,
    query: &str,
    results: Vec<SearchResult>,
    site_key: Option<String>,
) -> Response {
    if is_htmx {
        ResultsPartial {
            query: query.to_string(),
            results,
            turnstile_site_key: site_key,
        }
        .into_response()
    } else {
        SearchPage {
            query: query.to_string(),
            results,
            turnstile_site_key: site_key,
            message: None,
        }
        .into_response()
    }
}

async fn pdf_imslp(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let ip = audit::client_ip(&headers);
    let ua = audit::user_agent(&headers);

    let url = match state.imslp.fetch_pdf_url(&id).await {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(error = %e, id = %id, "imslp pdf url resolve failed");
            audit::record(
                &state.pool,
                &ip,
                ua.as_deref(),
                "pdf_imslp",
                Some(&id),
                "no_pdf_link",
                None,
            )
            .await;
            return (StatusCode::NOT_FOUND, "PDF not found").into_response();
        }
    };

    let upstream = match state.imslp.http().get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, url = %url, "imslp pdf fetch failed");
            audit::record(
                &state.pool,
                &ip,
                ua.as_deref(),
                "pdf_imslp",
                Some(&id),
                "upstream_unreachable",
                None,
            )
            .await;
            return (StatusCode::BAD_GATEWAY, "upstream fetch failed").into_response();
        }
    };

    if !upstream.status().is_success() {
        let status = upstream.status();
        audit::record(
            &state.pool,
            &ip,
            ua.as_deref(),
            "pdf_imslp",
            Some(&id),
            &format!("upstream_{}", status.as_u16()),
            None,
        )
        .await;
        return (StatusCode::BAD_GATEWAY, "upstream error").into_response();
    }

    let mut out_headers = HeaderMap::new();
    out_headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/pdf"));
    let fname = sanitize_filename(&id);
    if let Ok(v) = HeaderValue::from_str(&format!(r#"inline; filename="{fname}.pdf""#)) {
        out_headers.insert(header::CONTENT_DISPOSITION, v);
    }
    if let Some(len) = upstream
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| HeaderValue::from_str(s).ok())
    {
        out_headers.insert(header::CONTENT_LENGTH, len);
    }

    audit::record(
        &state.pool,
        &ip,
        ua.as_deref(),
        "pdf_imslp",
        Some(&id),
        "ok",
        None,
    )
    .await;

    let body = Body::from_stream(upstream.bytes_stream());
    (out_headers, body).into_response()
}

#[derive(Deserialize)]
struct EmailForm {
    imslp_id: String,
    title: String,
    recipient: String,
    #[serde(default, rename = "cf-turnstile-response")]
    turnstile_token: String,
    #[serde(default)]
    query: String,
}

async fn email_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<EmailForm>,
) -> Response {
    let ip = audit::client_ip(&headers);
    let ua = audit::user_agent(&headers);

    let secret = match settings::get_secret(
        &state.pool,
        &state.secrets,
        settings::TURNSTILE_SECRET_KEY,
    )
    .await
    {
        Ok(Some(s)) if !s.is_empty() => s,
        Ok(_) => {
            return flash(
                &form.query,
                "Email isn't configured yet. The admin needs to set the Turnstile secret key.",
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "turnstile secret read");
            return flash(&form.query, "Internal error reading settings.");
        }
    };

    if !turnstile::verify(state.imslp.http(), &secret, &form.turnstile_token, Some(&ip)).await {
        audit::record(
            &state.pool,
            &ip,
            ua.as_deref(),
            "email",
            Some(&form.recipient),
            "turnstile_failed",
            None,
        )
        .await;
        return flash(&form.query, "Verification failed. Try again.");
    }

    if !valid_email(&form.recipient) {
        return flash(&form.query, "Invalid email address.");
    }

    let buckets: [(String, i64); 3] = [
        ("global:email".to_string(), rate_limit::EMAIL_GLOBAL_PER_DAY),
        (
            format!("ip:{ip}:email"),
            rate_limit::EMAIL_PER_IP_PER_DAY,
        ),
        (
            format!("recipient:{}:email", form.recipient.to_lowercase()),
            rate_limit::EMAIL_PER_RECIPIENT_PER_DAY,
        ),
    ];
    for (bucket, limit) in &buckets {
        match rate_limit::check_and_increment(&state.pool, bucket, *limit).await {
            Ok(true) => {}
            Ok(false) => {
                audit::record(
                    &state.pool,
                    &ip,
                    ua.as_deref(),
                    "email",
                    Some(&form.recipient),
                    "rate_limited",
                    Some(&format!(r#"{{"bucket":"{}"}}"#, bucket)),
                )
                .await;
                return flash(
                    &form.query,
                    "Daily limit reached. Try again tomorrow or ask the admin to raise the cap.",
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "rate_limit check");
                return flash(&form.query, "Internal error.");
            }
        }
    }

    let smtp_cfg = match load_smtp_config(&state.pool, &state.secrets).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return flash(
                &form.query,
                "Email isn't configured yet. The admin needs to set SMTP credentials.",
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "smtp config load");
            return flash(&form.query, "Internal error loading SMTP config.");
        }
    };

    let pdf_bytes = match fetch_pdf_bytes(&state.imslp, &form.imslp_id).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, id = %form.imslp_id, "pdf fetch for email");
            audit::record(
                &state.pool,
                &ip,
                ua.as_deref(),
                "email",
                Some(&form.recipient),
                "pdf_fetch_failed",
                None,
            )
            .await;
            return flash(&form.query, "Could not fetch the PDF from IMSLP.");
        }
    };

    let subject = format!("Sheet music: {}", form.title);
    let body = format!(
        "Sheet music from IMSLP:\n\n{}\n\nDelivered via sheet-music-browsarr.",
        form.title
    );
    let filename = format!("{}.pdf", sanitize_filename(&form.title));

    match email_mod::send_pdf(&smtp_cfg, &form.recipient, &subject, &body, &filename, pdf_bytes)
        .await
    {
        Ok(()) => {
            audit::record(
                &state.pool,
                &ip,
                ua.as_deref(),
                "email",
                Some(&form.recipient),
                "ok",
                None,
            )
            .await;
            flash(
                &form.query,
                &format!("Sent! Check {}'s inbox.", form.recipient),
            )
        }
        Err(e) => {
            tracing::warn!(error = %e, "smtp send failed");
            audit::record(
                &state.pool,
                &ip,
                ua.as_deref(),
                "email",
                Some(&form.recipient),
                "smtp_error",
                None,
            )
            .await;
            flash(&form.query, "Failed to send email (SMTP error). Try again.")
        }
    }
}

async fn fetch_pdf_bytes(imslp: &Imslp, id: &str) -> anyhow::Result<Vec<u8>> {
    let url = imslp.fetch_pdf_url(id).await?;
    let resp = imslp
        .http()
        .get(&url)
        .send()
        .await?
        .error_for_status()?;
    if let Some(len) = resp.content_length() {
        anyhow::ensure!(
            (len as usize) <= MAX_PDF_BYTES,
            "PDF too large ({len} bytes; cap {MAX_PDF_BYTES})"
        );
    }
    let mut bytes = Vec::with_capacity(64 * 1024);
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if bytes.len() + chunk.len() > MAX_PDF_BYTES {
            anyhow::bail!("PDF exceeds {MAX_PDF_BYTES} bytes during streaming");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn load_smtp_config(
    pool: &SqlitePool,
    secrets: &Secrets,
) -> anyhow::Result<Option<SmtpConfig>> {
    let host = settings::get(pool, settings::SMTP_HOST).await.unwrap_or_default();
    let port: u16 = settings::get(pool, settings::SMTP_PORT)
        .await
        .and_then(|s| s.parse().ok())
        .unwrap_or(587);
    let user = settings::get(pool, settings::SMTP_USER).await.unwrap_or_default();
    let pass = settings::get_secret(pool, secrets, settings::SMTP_PASS)
        .await?
        .unwrap_or_default();
    let from = settings::get(pool, settings::SMTP_FROM).await.unwrap_or_default();

    if host.is_empty() || user.is_empty() || pass.is_empty() || from.is_empty() {
        return Ok(None);
    }
    Ok(Some(SmtpConfig {
        host,
        port,
        user,
        pass,
        from,
    }))
}

#[derive(Template)]
#[template(path = "flash.html")]
struct FlashTemplate {
    message: String,
    query: String,
}

fn flash(query: &str, message: &str) -> Response {
    FlashTemplate {
        message: message.to_string(),
        query: query.to_string(),
    }
    .into_response()
}

fn valid_email(s: &str) -> bool {
    let s = s.trim();
    if s.len() < 3 || s.len() > 320 {
        return false;
    }
    let at = match s.find('@') {
        Some(i) => i,
        None => return false,
    };
    if at == 0 || at == s.len() - 1 {
        return false;
    }
    if s.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    s[at + 1..].contains('.')
}

fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '-' | '_' => c,
            _ => '_',
        })
        .collect()
}

async fn login_page() -> impl IntoResponse {
    LoginTemplate { error: None }
}

#[derive(Deserialize)]
struct LoginForm {
    password: String,
}

async fn login_submit(
    State(state): State<AppState>,
    session: Session,
    Form(form): Form<LoginForm>,
) -> impl IntoResponse {
    match auth::verify_admin(&state.pool, &form.password).await {
        Ok(true) => {
            if let Err(e) = session.insert("admin", true).await {
                tracing::warn!(error = %e, "session insert failed");
            }
            Redirect::to("/admin").into_response()
        }
        Ok(false) => LoginTemplate {
            error: Some("Invalid password".into()),
        }
        .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "verify_admin failed");
            LoginTemplate {
                error: Some("Internal error".into()),
            }
            .into_response()
        }
    }
}

async fn logout(session: Session) -> impl IntoResponse {
    let _ = session.flush().await;
    Redirect::to("/login")
}
