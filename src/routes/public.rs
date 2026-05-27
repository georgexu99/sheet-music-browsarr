use std::collections::HashMap;

use askama::Template;
use askama_axum::IntoResponse;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::Deserialize;
use tower_sessions::Session;

use crate::audit;
use crate::auth;
use crate::sources::SearchResult;

use super::AppState;

#[derive(Template)]
#[template(path = "search.html")]
struct SearchPage {
    query: String,
    results: Vec<SearchResult>,
}

#[derive(Template)]
#[template(path = "search_results.html")]
struct ResultsPartial {
    query: String,
    results: Vec<SearchResult>,
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
        .route("/login", get(login_page).post(login_submit))
        .route("/logout", post(logout))
}

async fn home() -> impl IntoResponse {
    SearchPage {
        query: String::new(),
        results: Vec::new(),
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
        return render_results(is_htmx, &query, Vec::new()).into_response();
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

    render_results(is_htmx, &query, results).into_response()
}

fn render_results(is_htmx: bool, query: &str, results: Vec<SearchResult>) -> Response {
    if is_htmx {
        ResultsPartial {
            query: query.to_string(),
            results,
        }
        .into_response()
    } else {
        SearchPage {
            query: query.to_string(),
            results,
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
