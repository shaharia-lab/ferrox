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

    // Public routes (no auth)
    let public_routes = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics_handler));

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
