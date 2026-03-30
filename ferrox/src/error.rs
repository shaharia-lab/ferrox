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

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        let (status, error_type, message) = match &self {
            ProxyError::Unauthorized(msg) => {
                (StatusCode::UNAUTHORIZED, "unauthorized", msg.clone())
            }
            ProxyError::Forbidden(msg) => (StatusCode::FORBIDDEN, "forbidden", msg.clone()),
            ProxyError::ModelNotFound(msg) => {
                (StatusCode::NOT_FOUND, "model_not_found", msg.clone())
            }
            ProxyError::RateLimited(msg) => {
                (StatusCode::TOO_MANY_REQUESTS, "rate_limited", msg.clone())
            }
            ProxyError::CircuitOpen(msg) => (StatusCode::BAD_GATEWAY, "circuit_open", msg.clone()),
            ProxyError::ProviderError {
                status, message, ..
            } => {
                let http_status = StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY);
                (http_status, "provider_error", message.clone())
            }
            ProxyError::UpstreamTimeout(msg) => {
                (StatusCode::GATEWAY_TIMEOUT, "upstream_timeout", msg.clone())
            }
            ProxyError::StreamError(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "stream_error",
                msg.clone(),
            ),
            ProxyError::ConfigError(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "config_error",
                msg.clone(),
            ),
            ProxyError::SerializationError(e) => (
                StatusCode::BAD_REQUEST,
                "serialization_error",
                e.to_string(),
            ),
            ProxyError::HttpClientError(e) => {
                if e.is_timeout() {
                    (
                        StatusCode::GATEWAY_TIMEOUT,
                        "upstream_timeout",
                        e.to_string(),
                    )
                } else {
                    (StatusCode::BAD_GATEWAY, "http_client_error", e.to_string())
                }
            }
            ProxyError::AwsError(msg) => (StatusCode::BAD_GATEWAY, "aws_error", msg.clone()),
        };

        let body = json!({
            "error": {
                "message": message,
                "type": error_type,
                "code": status.as_u16()
            }
        });

        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
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
