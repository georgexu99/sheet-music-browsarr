use std::collections::HashMap;

use askama::Template;
use askama_axum::IntoResponse;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::{Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use futures_util::future::join_all;
use serde::Deserialize;
use sqlx::SqlitePool;
use tower_sessions::Session;

use crate::audit;
use crate::auth;
use crate::email::{self as email_mod, SmtpConfig};
use crate::i18n;
use crate::rate_limit;
use crate::secrets::Secrets;
use crate::settings;
use crate::sources::{SearchResult, Source};
use crate::turnstile;
use std::sync::Arc;

use super::AppState;

const MAX_PDF_BYTES: usize = 10 * 1024 * 1024;
const SEARCH_LIMIT_PER_SOURCE: usize = 10;
/// Typeahead: ignore queries shorter than this. Avoids 1-char query storms
/// while users are still typing the start of a name.
const SEARCH_MIN_QUERY_LEN: usize = 2;
/// Browser-side TTL for search responses. Re-typing the same query within
/// this window hits the browser HTTP cache instead of round-tripping. Sized
/// per the typeahead playbook (cache is stable enough for catalog content,
/// new items still surface within a minute).
const SEARCH_CACHE_TTL_SECS: u32 = 60;

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
        .route("/pdf/:source_id/:id", get(pdf_handler))
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
    if query.chars().count() < SEARCH_MIN_QUERY_LEN {
        // Typeahead: too short to be useful and the upstream sources will
        // mostly return noise. Return an empty result set silently — the
        // user will keep typing.
        return render_results(is_htmx, &query, Vec::new(), site_key).into_response();
    }

    // Multilingual expansion: a single user query becomes up to 4 query
    // variants (English / Simplified / Traditional / Pinyin) when known
    // composer or instrument names are recognised. See src/i18n/alias.rs.
    let variants = i18n::expand_query(&query);

    // Cross-product: (source, variant) -> one parallel search future.
    // Failing futures get a logged warn + empty Vec; nothing poisons the
    // whole search.
    let pairs: Vec<(Arc<dyn Source>, String)> = state
        .sources
        .iter()
        .flat_map(|s| {
            let src = s.clone();
            variants.iter().map(move |v| (src.clone(), v.clone()))
        })
        .collect();
    let futures = pairs.into_iter().map(|(src, v)| async move {
        match src.search(&v, SEARCH_LIMIT_PER_SOURCE).await {
            Ok(rs) => rs,
            Err(e) => {
                tracing::warn!(
                    source = src.id(),
                    variant = %v,
                    error = %e,
                    "source search failed"
                );
                Vec::new()
            }
        }
    });
    let groups = join_all(futures).await;

    // Dedupe across (source, id) — the same work commonly surfaces via
    // multiple variants (e.g. "Chopin" and "肖邦" both return it).
    let mut seen = std::collections::HashSet::new();
    let mut results = Vec::new();
    for group in groups {
        for r in group {
            if seen.insert((r.source.clone(), r.id.clone())) {
                results.push(r);
            }
        }
    }

    audit::record(
        &state.pool,
        &ip,
        ua.as_deref(),
        "search",
        Some(&query),
        "ok",
        Some(&format!(
            r#"{{"results":{},"variants":{}}}"#,
            results.len(),
            variants.len()
        )),
    )
    .await;

    let mut response = render_results(is_htmx, &query, results, site_key).into_response();
    // Browser HTTP cache: re-typing the same query within the TTL serves
    // from cache, no upstream re-hit. Same per-query response for any
    // anonymous visitor, so `public` is correct.
    if let Ok(v) = HeaderValue::from_str(&format!(
        "public, max-age={SEARCH_CACHE_TTL_SECS}"
    )) {
        response.headers_mut().insert(header::CACHE_CONTROL, v);
    }
    response
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

async fn pdf_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((source_id, id)): Path<(String, String)>,
) -> Response {
    let ip = audit::client_ip(&headers);
    let ua = audit::user_agent(&headers);

    let source = match state.find_source(&source_id) {
        Some(s) => s,
        None => {
            audit::record(
                &state.pool,
                &ip,
                ua.as_deref(),
                "pdf",
                Some(&format!("{source_id}/{id}")),
                "unknown_source",
                None,
            )
            .await;
            return (axum::http::StatusCode::NOT_FOUND, "Unknown source").into_response();
        }
    };

    match source.fetch_pdf_bytes(&id, MAX_PDF_BYTES).await {
        Ok(bytes) => {
            audit::record(
                &state.pool,
                &ip,
                ua.as_deref(),
                "pdf",
                Some(&format!("{source_id}/{id}")),
                "ok",
                Some(&format!(r#"{{"bytes":{}}}"#, bytes.len())),
            )
            .await;
            let mut out = HeaderMap::new();
            out.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/pdf"));
            let fname = sanitize_filename(&id);
            if let Ok(v) = HeaderValue::from_str(&format!(r#"inline; filename="{fname}.pdf""#)) {
                out.insert(header::CONTENT_DISPOSITION, v);
            }
            (out, bytes).into_response()
        }
        Err(e) => {
            tracing::warn!(
                source = %source_id,
                id = %id,
                error = %e,
                "fetch_pdf_bytes failed; falling back to external_url"
            );
            audit::record(
                &state.pool,
                &ip,
                ua.as_deref(),
                "pdf",
                Some(&format!("{source_id}/{id}")),
                "fetch_failed_redirect",
                None,
            )
            .await;
            Redirect::to(&source.external_url(&id)).into_response()
        }
    }
}

#[derive(Deserialize)]
struct EmailForm {
    source: String,
    item_id: String,
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

    let source = match state.find_source(&form.source) {
        Some(s) => s,
        None => return flash(&form.query, "Unknown source."),
    };

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

    // Reuse one of the sources' http clients for the Turnstile verify
    // call — any of them will do; they all carry sane timeouts.
    let verify_http = state
        .sources
        .first()
        .and_then(|_| Some(reqwest::Client::new()))
        .unwrap_or_default();
    if !turnstile::verify(&verify_http, &secret, &form.turnstile_token, Some(&ip)).await {
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
        (format!("ip:{ip}:email"), rate_limit::EMAIL_PER_IP_PER_DAY),
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

    let pdf_bytes = match source.fetch_pdf_bytes(&form.item_id, MAX_PDF_BYTES).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, source = %form.source, id = %form.item_id, "pdf fetch for email");
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
            return flash(&form.query, "Could not fetch the PDF.");
        }
    };

    let subject = format!("Sheet music: {}", form.title);
    let body = format!(
        "Sheet music from {}:\n\n{}\n\nDelivered via sheet-music-browsarr.",
        source.display_name(),
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
