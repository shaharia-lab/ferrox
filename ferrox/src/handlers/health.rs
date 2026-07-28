use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use std::sync::atomic::Ordering;

use crate::state::AppState;

#[utoipa::path(
    get,
    path = "/healthz",
    tag = "Observability",
    responses((status = 200, description = "Process is alive"))
)]
pub async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({ "status": "ok" })))
}

#[utoipa::path(
    get,
    path = "/readyz",
    tag = "Observability",
    responses(
        (status = 200, description = "Ready to serve traffic"),
        (status = 503, description = "Not ready (still starting or draining)")
    )
)]
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
