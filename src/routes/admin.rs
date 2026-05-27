use askama::Template;
use askama_axum::IntoResponse;
use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::{Redirect, Response};
use axum::routing::get;
use axum::Router;
use tower_sessions::Session;

use super::AppState;

#[derive(Template)]
#[template(path = "admin/index.html")]
struct AdminIndex;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin", get(index))
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
