use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use std::sync::atomic::Ordering;

use crate::state::AppState;

pub async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({ "status": "ok" })))
}

pub async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    if state.ready.load(Ordering::Acquire) {
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "ready",
                "version": env!("CARGO_PKG_VERSION")
            })),
        )
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "status": "not ready" })),
        )
    }
}
