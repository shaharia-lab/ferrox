use axum::{
    extract::{DefaultBodyLimit, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
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
    responses::responses,
};
use crate::state::AppState;
use crate::telemetry::metrics::gather as gather_metrics;

pub fn build_router(state: AppState) -> Router {
    let request_timeout = Duration::from_secs(state.config.server.timeouts.ttfb_secs + 3600);

    // OpenAI-compatible routes (Authorization: Bearer)
    let v1_routes = Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/responses", post(responses))
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
        // `server.max_request_body_bytes` replaces axum's built-in 2 MB
        // extractor cap on every route (`Json` and `Bytes` both honor it).
        // The middleware sits outside auth, so a declared-oversize body is
        // refused before any work, and rewrites axum's plain-text 413 into
        // the route surface's JSON error shape.
        .layer(DefaultBodyLimit::max(
            state.config.server.max_request_body_bytes,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            body_limit_middleware,
        ))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(TimeoutLayer::new(request_timeout))
        .with_state(state)
}

/// Enforces `server.max_request_body_bytes` with a JSON 413.
///
/// A `Content-Length` above the limit is rejected up front. A body without one
/// (chunked) is cut off by the extractor's [`DefaultBodyLimit`], whose
/// plain-text 413 is rewritten here. JSON 413s (e.g. relayed from a provider)
/// pass through untouched.
async fn body_limit_middleware(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let limit = state.config.server.max_request_body_bytes;
    let anthropic = request.uri().path().starts_with("/anthropic/");

    let declared = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|len| len > limit as u64) {
        return payload_too_large(limit, anthropic);
    }

    let response = next.run(request).await;
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if response.status() == StatusCode::PAYLOAD_TOO_LARGE && !is_json {
        return payload_too_large(limit, anthropic);
    }
    response
}

fn payload_too_large(limit: usize, anthropic: bool) -> Response {
    let message = format!("request body exceeds {limit} bytes");
    let body = if anthropic {
        serde_json::json!({
            "type": "error",
            "error": {"type": "request_too_large", "message": message}
        })
    } else {
        serde_json::json!({
            "error": {"message": message, "type": "invalid_request_error", "code": 413}
        })
    };
    (StatusCode::PAYLOAD_TOO_LARGE, Json(body)).into_response()
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
        build_state_with(config, std::collections::HashMap::new())
    }

    fn build_state_with(config: Config, registry: crate::providers::ProviderRegistry) -> AppState {
        let router = crate::router::ModelRouter::from_config(&config, &registry).unwrap();
        let resolver = crate::classifier::RouteResolver::build(&config, router).unwrap();
        let jwks_cache = crate::jwks::JwksCache::new(vec![], 300, reqwest::Client::new());
        AppState {
            config: Arc::new(config),
            providers: Arc::new(registry),
            resolver: Arc::new(resolver),
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

    /// A gateway with two statically routed aliases and `auto`, classified
    /// between them. The virtual key `sk-test` may use every alias.
    async fn classified_app() -> Router {
        let config: Config = serde_json::from_value(serde_json::json!({
            "providers": [{"name": "mock", "type": "openai", "api_key": "k"}],
            "classifiers": [{"id": "jev-main", "type": "jev", "api_key": "k"}],
            "models": [
                {"alias": "fast", "routing": {"strategy": "failover", "targets": [{"provider": "mock", "model_id": "s"}]}},
                {"alias": "smart", "routing": {"strategy": "failover", "targets": [{"provider": "mock", "model_id": "l"}]}},
                {"alias": "auto", "classifier": {
                    "use": "jev-main",
                    "fallback_alias": "smart",
                    "tiers": {"simple": {"alias": "fast", "when": "short"}}}},
            ],
            "virtual_keys": [{"key": "sk-test", "name": "test", "allowed_models": ["*"]}],
        }))
        .unwrap();
        crate::config::validate(&config).unwrap();

        let registry = crate::providers::build_registry(&config.providers, &config.defaults)
            .await
            .unwrap();
        build_router(build_state_with(config, registry))
    }

    async fn send(app: Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
        let resp = app.oneshot(request).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn model_listings_include_classified_aliases() {
        for path in ["/v1/models", "/anthropic/v1/models"] {
            let request = Request::builder()
                .uri(path)
                .header("authorization", "Bearer sk-test")
                .body(Body::empty())
                .unwrap();
            let (status, body) = send(classified_app().await, request).await;
            assert_eq!(status, StatusCode::OK, "{path}");
            let ids: Vec<&str> = body["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["id"].as_str().unwrap())
                .collect();
            assert_eq!(ids, ["fast", "smart", "auto"], "{path}");
        }
    }

    // ── Request body limit ──────────────────────────────────────────────────

    /// App with one virtual key and no models, so a body that gets past the
    /// size limit and parses fails later with a non-413 status.
    fn body_limit_app(limit: Option<usize>) -> Router {
        let mut config: Config = serde_json::from_value(serde_json::json!({
            "providers": [],
            "models": [],
            "virtual_keys": [{"key": "sk-test", "name": "test", "allowed_models": ["*"]}],
        }))
        .unwrap();
        if let Some(limit) = limit {
            config.server.max_request_body_bytes = limit;
        }
        build_router(build_state(config))
    }

    /// A valid chat-completions JSON body padded to roughly `size` bytes.
    fn chat_body(size: usize) -> Vec<u8> {
        let padding = "a".repeat(size);
        serde_json::to_vec(&serde_json::json!({
            "model": "missing",
            "messages": [{"role": "user", "content": padding}],
        }))
        .unwrap()
    }

    fn post(path: &str, body: Vec<u8>, with_content_length: bool) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri(path)
            .header("authorization", "Bearer sk-test")
            .header("content-type", "application/json");
        if with_content_length {
            builder = builder.header("content-length", body.len());
        }
        builder.body(Body::from(body)).unwrap()
    }

    #[test]
    fn default_body_limit_is_32_mib() {
        assert_eq!(
            minimal_config().server.max_request_body_bytes,
            32 * 1024 * 1024
        );
    }

    #[tokio::test]
    async fn body_between_10_and_32_mib_is_accepted_on_every_post_route() {
        let body = chat_body(20 * 1024 * 1024);
        for path in [
            "/v1/chat/completions",
            "/v1/responses",
            "/anthropic/v1/messages",
        ] {
            for with_len in [true, false] {
                let resp = body_limit_app(None)
                    .oneshot(post(path, body.clone(), with_len))
                    .await
                    .unwrap();
                assert_ne!(
                    resp.status(),
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "{path} (content-length: {with_len}) rejected a 20 MiB body"
                );
            }
        }
    }

    #[tokio::test]
    async fn body_over_limit_gets_json_413_naming_the_limit() {
        let limit = 1024 * 1024;
        let body = chat_body(2 * limit);
        for path in [
            "/v1/chat/completions",
            "/v1/responses",
            "/anthropic/v1/messages",
        ] {
            // With Content-Length (rejected up front) and without (chunked,
            // cut off by the extractor).
            for with_len in [true, false] {
                let (status, json) = send(
                    body_limit_app(Some(limit)),
                    post(path, body.clone(), with_len),
                )
                .await;
                assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{path} {with_len}");
                assert_eq!(
                    json["error"]["message"],
                    format!("request body exceeds {limit} bytes"),
                    "{path} {with_len}"
                );
                if path.starts_with("/anthropic/") {
                    assert_eq!(json["type"], "error");
                    assert_eq!(json["error"]["type"], "request_too_large");
                } else {
                    assert_eq!(json["error"]["type"], "invalid_request_error");
                    assert_eq!(json["error"]["code"], 413);
                }
            }
        }
    }

    #[tokio::test]
    async fn oversize_declared_body_is_rejected_before_auth() {
        let limit = 1024;
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-length", limit + 1)
            .body(Body::from(vec![b'x'; limit + 1]))
            .unwrap();
        let (status, _) = send(body_limit_app(Some(limit)), request).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }
}
