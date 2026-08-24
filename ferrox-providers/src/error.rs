#[cfg(feature = "axum")]
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
#[allow(dead_code)] // RateLimited/CircuitOpen used in Phase 2
pub enum ProxyError {
    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    #[error("Forbidden: {0}")]
    Forbidden(String),

    #[error("Model not found: {0}")]
    ModelNotFound(String),

    #[error("Rate limited: {0}")]
    RateLimited(String),

    #[error("Budget exceeded: {0}")]
    BudgetExceeded(String),

    #[error("Circuit open: {0}")]
    CircuitOpen(String),

    #[error("Provider error from {provider} (status {status}): {message}")]
    ProviderError {
        provider: String,
        status: u16,
        message: String,
    },

    #[error("Upstream timeout: {0}")]
    UpstreamTimeout(String),

    #[error("Stream error: {0}")]
    StreamError(String),

    #[error("Config error: {0}")]
    ConfigError(String),

    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),

    #[error("HTTP client error: {0}")]
    HttpClientError(#[from] reqwest::Error),

    #[error("AWS error: {0}")]
    AwsError(String),
}

/// `StatusCode::from_u16` accepts 100..=999; anything else is not a valid HTTP
/// status and falls back to 502, matching what the axum layer did when it owned
/// this decision.
fn http_status_or_bad_gateway(status: u16) -> u16 {
    if (100..=999).contains(&status) {
        status
    } else {
        502
    }
}

/// Maps every variant onto an OpenAI-shaped JSON error body —
/// `{"error":{"message":…,"type":…,"code":…}}` — plus the HTTP status to serve
/// it with.
///
/// Framework-free on purpose: a consumer builds its own response from the pair,
/// and the `axum` feature's `IntoResponse` is just one such consumer.
pub fn openai_error_body(e: &ProxyError) -> (u16, serde_json::Value) {
    let (status, error_type, message) = match e {
        ProxyError::Unauthorized(msg) => (401, "unauthorized", msg.clone()),
        ProxyError::Forbidden(msg) => (403, "forbidden", msg.clone()),
        ProxyError::ModelNotFound(msg) => (404, "model_not_found", msg.clone()),
        ProxyError::RateLimited(msg) => (429, "rate_limited", msg.clone()),
        ProxyError::BudgetExceeded(msg) => (429, "budget_exceeded", msg.clone()),
        ProxyError::CircuitOpen(msg) => (502, "circuit_open", msg.clone()),
        ProxyError::ProviderError {
            status, message, ..
        } => (
            http_status_or_bad_gateway(*status),
            "provider_error",
            message.clone(),
        ),
        ProxyError::UpstreamTimeout(msg) => (504, "upstream_timeout", msg.clone()),
        ProxyError::StreamError(msg) => (500, "stream_error", msg.clone()),
        ProxyError::ConfigError(msg) => (500, "config_error", msg.clone()),
        ProxyError::SerializationError(e) => (400, "serialization_error", e.to_string()),
        ProxyError::HttpClientError(e) => {
            if e.is_timeout() {
                (504, "upstream_timeout", e.to_string())
            } else {
                (502, "http_client_error", e.to_string())
            }
        }
        ProxyError::AwsError(msg) => (502, "aws_error", msg.clone()),
    };

    (
        status,
        json!({
            "error": {
                "message": message,
                "type": error_type,
                "code": status
            }
        }),
    )
}

/// Maps every variant onto an Anthropic-shaped JSON error body —
/// `{"type":"error","error":{"type":…,"message":…}}` — plus the HTTP status.
///
/// Anthropic SDK clients branch on `error.type` (`authentication_error` vs
/// `permission_error` vs `overloaded_error` drive real retry behavior), so any
/// gateway exposing an Anthropic-native surface must emit exactly these strings.
/// Returning the status alongside the body lets the non-streaming response path
/// and the mid-stream SSE `error` event share one mapping.
pub fn anthropic_error_body(e: &ProxyError) -> (u16, serde_json::Value) {
    let (status, error_type, message) = match e {
        ProxyError::Unauthorized(msg) => (401, "authentication_error", msg.clone()),
        ProxyError::Forbidden(msg) => (403, "permission_error", msg.clone()),
        ProxyError::ModelNotFound(msg) => (404, "not_found_error", msg.clone()),
        ProxyError::RateLimited(msg) | ProxyError::BudgetExceeded(msg) => {
            (429, "rate_limit_error", msg.clone())
        }
        // 529 is Anthropic's "overloaded" status; 502 is the closest standard code.
        ProxyError::CircuitOpen(msg) => (502, "overloaded_error", msg.clone()),
        ProxyError::ProviderError {
            status, message, ..
        } => (
            http_status_or_bad_gateway(*status),
            "api_error",
            message.clone(),
        ),
        ProxyError::UpstreamTimeout(msg) => (504, "api_error", msg.clone()),
        ProxyError::StreamError(msg) => (500, "api_error", msg.clone()),
        other => (500, "api_error", other.to_string()),
    };

    (
        status,
        json!({
            "type": "error",
            "error": {
                "type": error_type,
                "message": message
            }
        }),
    )
}

/// Serves [`openai_error_body`] as an axum response.
///
/// Gated on the `axum` feature so the crate does not force a web-framework
/// version on consumers that only want the translation layer.
#[cfg(feature = "axum")]
impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        let (status, body) = openai_error_body(&self);
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
        (status, Json(body)).into_response()
    }
}

// The mapping functions are framework-free, so their tests are too — they run in
// the lean build and are the byte-identity contract both surfaces are held to.
#[cfg(test)]
mod body_tests {
    use super::*;

    fn provider_error(status: u16) -> ProxyError {
        ProxyError::ProviderError {
            provider: "anthropic".to_string(),
            status,
            message: "overloaded".to_string(),
        }
    }

    fn serde_error() -> ProxyError {
        ProxyError::SerializationError(serde_json::from_str::<i32>("nope").unwrap_err())
    }

    // ── OpenAI shape ─────────────────────────────────────────────────────────

    #[test]
    fn openai_shape_covers_every_variant() {
        // (error, expected status, expected `error.type`)
        let cases: Vec<(ProxyError, u16, &str)> = vec![
            (ProxyError::Unauthorized("k".into()), 401, "unauthorized"),
            (ProxyError::Forbidden("k".into()), 403, "forbidden"),
            (
                ProxyError::ModelNotFound("m".into()),
                404,
                "model_not_found",
            ),
            (ProxyError::RateLimited("r".into()), 429, "rate_limited"),
            (
                ProxyError::BudgetExceeded("b".into()),
                429,
                "budget_exceeded",
            ),
            (ProxyError::CircuitOpen("c".into()), 502, "circuit_open"),
            (provider_error(503), 503, "provider_error"),
            (
                ProxyError::UpstreamTimeout("t".into()),
                504,
                "upstream_timeout",
            ),
            (ProxyError::StreamError("s".into()), 500, "stream_error"),
            (ProxyError::ConfigError("c".into()), 500, "config_error"),
            (serde_error(), 400, "serialization_error"),
            (ProxyError::AwsError("a".into()), 502, "aws_error"),
        ];

        for (err, want_status, want_type) in cases {
            let (status, body) = openai_error_body(&err);
            assert_eq!(status, want_status, "status for {want_type}");
            assert_eq!(body["error"]["type"], want_type);
            // `code` mirrors the HTTP status — clients read it instead of the header.
            assert_eq!(body["error"]["code"], want_status);
            assert!(
                body["error"]["message"].is_string(),
                "message present for {want_type}"
            );
        }
    }

    #[test]
    fn openai_shape_carries_the_message_verbatim() {
        let (_, body) = openai_error_body(&ProxyError::Forbidden("test msg".into()));
        assert_eq!(body["error"]["message"], "test msg");
    }

    // ── Anthropic shape ──────────────────────────────────────────────────────

    #[test]
    fn anthropic_shape_covers_every_variant() {
        let cases: Vec<(ProxyError, u16, &str)> = vec![
            (
                ProxyError::Unauthorized("k".into()),
                401,
                "authentication_error",
            ),
            (ProxyError::Forbidden("k".into()), 403, "permission_error"),
            (
                ProxyError::ModelNotFound("m".into()),
                404,
                "not_found_error",
            ),
            (ProxyError::RateLimited("r".into()), 429, "rate_limit_error"),
            (
                ProxyError::BudgetExceeded("b".into()),
                429,
                "rate_limit_error",
            ),
            (ProxyError::CircuitOpen("c".into()), 502, "overloaded_error"),
            (provider_error(503), 503, "api_error"),
            (ProxyError::UpstreamTimeout("t".into()), 504, "api_error"),
            (ProxyError::StreamError("s".into()), 500, "api_error"),
            // The catch-all arm: everything else is a 500 api_error.
            (ProxyError::ConfigError("c".into()), 500, "api_error"),
            (serde_error(), 500, "api_error"),
            (ProxyError::AwsError("a".into()), 500, "api_error"),
        ];

        for (err, want_status, want_type) in cases {
            let (status, body) = anthropic_error_body(&err);
            assert_eq!(status, want_status, "status for {err}");
            assert_eq!(body["type"], "error", "envelope for {err}");
            assert_eq!(body["error"]["type"], want_type, "type for {err}");
            assert!(body["error"]["message"].is_string(), "message for {err}");
        }
    }

    #[test]
    fn anthropic_shape_uses_the_upstream_message_for_provider_errors() {
        // Not the full `Display` (which would prefix "Provider error from …") —
        // clients see what the upstream said.
        let (_, body) = anthropic_error_body(&provider_error(503));
        assert_eq!(body["error"]["message"], "overloaded");
    }

    #[test]
    fn anthropic_shape_omits_the_openai_code_field() {
        let (_, body) = anthropic_error_body(&ProxyError::Forbidden("k".into()));
        assert!(
            body["error"].get("code").is_none(),
            "Anthropic's shape has no `code`: {body}"
        );
    }

    // ── Status handling, shared by both shapes ───────────────────────────────

    #[test]
    fn provider_error_passes_its_own_status_through() {
        for status in [400u16, 429, 503, 999] {
            assert_eq!(openai_error_body(&provider_error(status)).0, status);
            assert_eq!(anthropic_error_body(&provider_error(status)).0, status);
        }
    }

    #[test]
    fn provider_error_with_an_impossible_status_falls_back_to_502() {
        // `StatusCode::from_u16` accepts 100..=999; the guard must reject the rest
        // exactly as the axum layer did when it owned this decision.
        for status in [0u16, 42, 1000, u16::MAX] {
            assert_eq!(
                openai_error_body(&provider_error(status)).0,
                502,
                "openai fallback for {status}"
            );
            assert_eq!(
                anthropic_error_body(&provider_error(status)).0,
                502,
                "anthropic fallback for {status}"
            );
        }
    }
}

// Every test here asserts on the HTTP mapping, so the module follows the impl.
#[cfg(all(test, feature = "axum"))]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn response_parts(err: ProxyError) -> (u16, serde_json::Value) {
        let resp = err.into_response();
        let status = resp.status().as_u16();
        let body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        (status, json)
    }

    #[tokio::test]
    async fn unauthorized_is_401() {
        let (status, json) = response_parts(ProxyError::Unauthorized("bad key".into())).await;
        assert_eq!(status, 401);
        assert_eq!(json["error"]["type"], "unauthorized");
        assert_eq!(json["error"]["code"], 401);
    }

    #[tokio::test]
    async fn forbidden_is_403() {
        let (status, json) = response_parts(ProxyError::Forbidden("no access".into())).await;
        assert_eq!(status, 403);
        assert_eq!(json["error"]["type"], "forbidden");
        assert_eq!(json["error"]["code"], 403);
    }

    #[tokio::test]
    async fn model_not_found_is_404() {
        let (status, json) = response_parts(ProxyError::ModelNotFound("gpt-5".into())).await;
        assert_eq!(status, 404);
        assert_eq!(json["error"]["type"], "model_not_found");
    }

    #[tokio::test]
    async fn rate_limited_is_429() {
        let (status, json) = response_parts(ProxyError::RateLimited("slow down".into())).await;
        assert_eq!(status, 429);
        assert_eq!(json["error"]["type"], "rate_limited");
    }

    #[tokio::test]
    async fn circuit_open_is_502() {
        let (status, json) = response_parts(ProxyError::CircuitOpen("open".into())).await;
        assert_eq!(status, 502);
        assert_eq!(json["error"]["type"], "circuit_open");
    }

    #[tokio::test]
    async fn provider_error_uses_its_own_status() {
        let err = ProxyError::ProviderError {
            provider: "anthropic".to_string(),
            status: 503,
            message: "overloaded".to_string(),
        };
        let (status, json) = response_parts(err).await;
        assert_eq!(status, 503);
        assert_eq!(json["error"]["type"], "provider_error");
        assert_eq!(json["error"]["message"], "overloaded");
    }

    #[tokio::test]
    async fn upstream_timeout_is_504() {
        let (status, json) = response_parts(ProxyError::UpstreamTimeout("timed out".into())).await;
        assert_eq!(status, 504);
        assert_eq!(json["error"]["type"], "upstream_timeout");
    }

    #[tokio::test]
    async fn stream_error_is_500() {
        let (status, json) = response_parts(ProxyError::StreamError("broken pipe".into())).await;
        assert_eq!(status, 500);
        assert_eq!(json["error"]["type"], "stream_error");
    }

    #[tokio::test]
    async fn config_error_is_500() {
        let (status, json) = response_parts(ProxyError::ConfigError("bad config".into())).await;
        assert_eq!(status, 500);
        assert_eq!(json["error"]["type"], "config_error");
    }

    #[tokio::test]
    async fn aws_error_is_502() {
        let (status, json) = response_parts(ProxyError::AwsError("bedrock down".into())).await;
        assert_eq!(status, 502);
        assert_eq!(json["error"]["type"], "aws_error");
    }

    #[tokio::test]
    async fn error_body_has_message_field() {
        let (_, json) = response_parts(ProxyError::Forbidden("test msg".into())).await;
        assert_eq!(json["error"]["message"], "test msg");
    }

    #[tokio::test]
    async fn provider_error_invalid_status_falls_back_to_502() {
        // StatusCode::from_u16 requires 100-999; values outside this range are invalid.
        // axum's StatusCode::from_u16(0) returns Err, so our code falls back to 502.
        let err = ProxyError::ProviderError {
            provider: "test".to_string(),
            status: 0, // truly invalid — triggers the unwrap_or(BAD_GATEWAY) fallback
            message: "weird".to_string(),
        };
        let (status, _) = response_parts(err).await;
        assert_eq!(status, 502);
    }
}
