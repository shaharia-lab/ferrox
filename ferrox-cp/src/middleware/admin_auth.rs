use axum::{
    body::Body,
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

use crate::state::CpState;

/// Axum middleware that guards all `/api/*` routes with the static admin key.
///
/// Expects `Authorization: Bearer <CP_ADMIN_KEY>`.  Returns `401` on any
/// mismatch or missing header — no timing information is leaked because the
/// comparison uses a constant-time equality check.
pub async fn require_admin_key(
    State(state): State<CpState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let ok = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|token| constant_time_eq(token.as_bytes(), state.config.admin_key.as_bytes()))
        .unwrap_or(false);

    if !ok {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized", "message": "valid CP_ADMIN_KEY required"})),
        )
            .into_response();
    }

    next.run(request).await
}

/// Constant-time byte slice comparison to prevent timing-based secret enumeration.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}
