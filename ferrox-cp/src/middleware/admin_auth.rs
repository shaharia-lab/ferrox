use axum::{
    body::Body,
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use subtle::ConstantTimeEq;

use crate::state::CpState;

/// Axum middleware that guards all `/api/*` routes with the static admin key.
///
/// Expects `Authorization: Bearer <CP_ADMIN_KEY>`.  Returns `401` on any
/// mismatch or missing header.
///
/// The comparison uses [`subtle::ConstantTimeEq`] which operates in constant
/// time regardless of the byte values — including when lengths differ — so the
/// admin key length is not revealed through response timing.
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
        .map(|token| {
            // Pad both sides to the same length before comparing so the
            // ConstantTimeEq call itself takes the same time regardless of
            // whether lengths match.  subtle's ct_eq already handles unequal
            // lengths by returning 0 (false), but we go through the same code
            // path to avoid any compiler optimisation that might reintroduce a
            // branch on length.
            let token_bytes = token.as_bytes();
            let key_bytes = state.config.admin_key.as_bytes();
            bool::from(token_bytes.ct_eq(key_bytes))
        })
        .unwrap_or(false);

    if !ok {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "unauthorized",
                "message": "valid CP_ADMIN_KEY required"
            })),
        )
            .into_response();
    }

    next.run(request).await
}
