use askama::Template;
use askama_axum::IntoResponse;
use axum::extract::State;
use axum::response::Redirect;
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::Deserialize;
use tower_sessions::Session;

use crate::auth;

use super::AppState;

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    error: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(home))
        .route("/healthz", get(healthz))
        .route("/login", get(login_page).post(login_submit))
        .route("/logout", post(logout))
}

async fn home() -> &'static str {
    "sheet-music-browsarr"
}

async fn healthz() -> &'static str {
    "ok"
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
