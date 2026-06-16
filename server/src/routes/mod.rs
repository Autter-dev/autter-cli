//! HTTP routing. Paths match exactly what the autter CLI calls
//! (`{api_base_url}/worker/...`).

pub mod cas;
pub mod notes;

use axum::routing::{get, post};
use axum::Router;

use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/worker/notes/upload", post(notes::upload))
        .route("/worker/notes/", get(notes::read))
        .route("/worker/cas/upload", post(cas::upload))
        .route("/worker/cas/", get(cas::read))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}
