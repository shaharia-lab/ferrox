use axum::{
    middleware,
    routing::{get, post},
    Router,
};
use std::time::Duration;
use tower_http::{
    cors::CorsLayer,
    request_id::{MakeRequestUuid, SetRequestIdLayer},
    timeout::TimeoutLayer,
    trace::TraceLayer,
};

use crate::auth::auth_middleware;
use crate::handlers::{
    anthropic_messages::anthropic_messages,
    anthropic_models::list_models_anthropic,
    chat::chat_completions,
    health::{healthz, readyz},
    models::list_models,
};
use crate::state::AppState;
use crate::telemetry::metrics::gather as gather_metrics;

pub fn build_router(state: AppState) -> Router {
    let request_timeout = Duration::from_secs(state.config.server.timeouts.ttfb_secs + 3600);

    // OpenAI-compatible routes (Authorization: Bearer)
    let v1_routes = Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/models", get(list_models))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    // Anthropic-native routes (x-api-key or Authorization: Bearer)
    let anthropic_routes = Router::new()
        .route("/anthropic/v1/messages", post(anthropic_messages))
        .route("/anthropic/v1/models", get(list_models_anthropic))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    // Public routes (no auth). The metrics endpoint is mounted only when
    // enabled, at its configured path — honoring `telemetry.metrics`
    // (`enabled` + `path`), which was previously ignored (the route was
    // hardcoded at `/metrics` and always mounted).
    let mut public_routes = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        // OpenAPI schema for the gateway surface — discovery/SDK-generation for
        // gateway-only deployments. Cold, unauthenticated; `/openapi.json` is
        // the auto-detected convention, `/api-schema` a friendly alias.
        .route("/api-schema", get(schema_handler))
        .route("/openapi.json", get(schema_handler));
    let metrics = &state.config.telemetry.metrics;
    if metrics.enabled {
        public_routes = public_routes.route(&metrics.path, get(metrics_handler));
    }

    Router::new()
        .merge(v1_routes)
        .merge(anthropic_routes)
        .merge(public_routes)
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(TimeoutLayer::new(request_timeout))
        .with_state(state)
}

async fn metrics_handler() -> impl axum::response::IntoResponse {
    let body = gather_metrics();
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
}

/// Serve the pre-built OpenAPI document as `application/json`. The body is
/// cached (built once), so this route allocates nothing per request.
async fn schema_handler() -> impl axum::response::IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        crate::openapi::openapi_json(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tower::ServiceExt; // for `oneshot`

    /// Build a minimal `AppState` for router tests. All config fields default
    /// except `providers`/`models` (required by deserialization); the caller
    /// tweaks `telemetry.metrics` before calling.
    fn build_state(config: Config) -> AppState {
        let registry: crate::providers::ProviderRegistry = std::collections::HashMap::new();
        let router = crate::router::ModelRouter::from_config(&config, &registry).unwrap();
        let jwks_cache = crate::jwks::JwksCache::new(vec![], 300, reqwest::Client::new());
        AppState {
            config: Arc::new(config),
            providers: Arc::new(registry),
            router: Arc::new(router),
            rate_limit_backend: Arc::new(crate::ratelimit::MemoryBackend::new()),
            metrics: Arc::new(crate::metrics::Metrics::new()),
            ready: Arc::new(AtomicBool::new(true)),
            jwks_cache: Arc::new(jwks_cache),
            usage_writer: crate::usage_writer::noop_writer(),
            budget_enforcer: Arc::new(crate::budget_enforcer::NoopBudgetEnforcer),
            event_dispatcher: crate::event_dispatcher::noop_dispatcher(),
        }
    }

    fn minimal_config() -> Config {
        serde_json::from_value(serde_json::json!({"providers": [], "models": []})).unwrap()
    }

    /// GET `path` against the built router, returning the response status.
    async fn get_status(config: Config, path: &str) -> StatusCode {
        let app = build_router(build_state(config));
        let resp = app
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        resp.status()
    }

    #[tokio::test]
    async fn metrics_route_served_at_default_path_when_enabled() {
        // Default config: enabled = true, path = "/metrics".
        assert_eq!(
            get_status(minimal_config(), "/metrics").await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn metrics_route_absent_when_disabled() {
        let mut config = minimal_config();
        config.telemetry.metrics.enabled = false;
        assert_eq!(
            get_status(config, "/metrics").await,
            StatusCode::NOT_FOUND,
            "metrics route must not be mounted when disabled"
        );
    }

    #[tokio::test]
    async fn metrics_route_served_at_custom_path() {
        let mut config = minimal_config();
        config.telemetry.metrics.path = "/internal/metrics".to_string();
        // Served at the custom path...
        assert_eq!(
            get_status(config.clone(), "/internal/metrics").await,
            StatusCode::OK
        );
        // ...and no longer at the hardcoded default.
        assert_eq!(get_status(config, "/metrics").await, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn health_routes_unaffected_by_metrics_config() {
        let mut config = minimal_config();
        config.telemetry.metrics.enabled = false;
        assert_eq!(get_status(config.clone(), "/healthz").await, StatusCode::OK);
        assert_eq!(get_status(config, "/readyz").await, StatusCode::OK);
    }

    /// GET `path`, returning (status, content-type, body bytes).
    async fn get_full(config: Config, path: &str) -> (StatusCode, String, Vec<u8>) {
        let app = build_router(build_state(config));
        let resp = app
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let content_type = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        (status, content_type, bytes)
    }

    #[tokio::test]
    async fn schema_routes_serve_openapi_json_without_auth() {
        for path in ["/api-schema", "/openapi.json"] {
            let (status, content_type, body) = get_full(minimal_config(), path).await;
            assert_eq!(status, StatusCode::OK, "{path} should be public + 200");
            assert!(
                content_type.starts_with("application/json"),
                "{path} content-type was {content_type}"
            );
            let doc: serde_json::Value = serde_json::from_slice(&body)
                .unwrap_or_else(|e| panic!("{path} body must be JSON: {e}"));
            assert!(
                doc["openapi"]
                    .as_str()
                    .unwrap_or_default()
                    .starts_with("3."),
                "{path} must be an OpenAPI 3.x document"
            );
            assert!(doc["paths"]["/v1/chat/completions"].is_object());
        }
    }
}
