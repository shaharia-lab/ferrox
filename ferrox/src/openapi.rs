//! OpenAPI 3.1 schema for the gateway's HTTP surface.
//!
//! The document is built at compile time by `utoipa` from the `#[utoipa::path]`
//! annotations on the handlers plus the `ToSchema`-deriving types, and served
//! from a cold, unauthenticated `/schema` (alias `/openapi.json`) route — no
//! hot-path impact.
//!
//! Scope guardrail: the `/v1/chat/completions` and `/anthropic/v1/messages`
//! bodies mirror the upstream OpenAI / Anthropic wire formats (streaming deltas,
//! tool-call unions, multimodal content blocks). Reproducing those in full would
//! be huge and would drift, so only the fields Ferrox itself owns/reads are
//! modeled here, with a pointer to the upstream spec for the exhaustive tail.
//! The shapes Ferrox fully owns (`/v1/models`, `/anthropic/v1/models`, the error
//! envelope) are modeled completely.

use once_cell::sync::Lazy;
use serde::Serialize;
use utoipa::openapi::security::{ApiKey, ApiKeyValue, Http, HttpAuthScheme, SecurityScheme};
use utoipa::{Modify, OpenApi, ToSchema};

/// The gateway error envelope: every error response is `{"error": {...}}`.
/// Mirrors the shape produced by `crate::error::ProxyError`.
#[derive(Serialize, ToSchema)]
pub struct ErrorResponse {
    pub error: ErrorDetail,
}

#[derive(Serialize, ToSchema)]
pub struct ErrorDetail {
    /// Human-readable error message.
    #[schema(example = "Model not found: gpt-5")]
    pub message: String,
    /// Machine-readable error category (e.g. `unauthorized`, `model_not_found`,
    /// `rate_limited`, `provider_error`).
    #[schema(rename = "type", example = "model_not_found")]
    pub error_type: String,
    /// HTTP status code, duplicated in the body for convenience.
    #[schema(example = 404)]
    pub code: u16,
}

/// Core of the OpenAI-format chat request. Ferrox forwards the **full** OpenAI
/// Chat Completions body upstream; only the fields the gateway reads/owns are
/// modeled. See <https://platform.openai.com/docs/api-reference/chat/create>
/// for the exhaustive schema (messages content unions, tools, etc.).
///
/// Schema-only: the fields exist to describe the request body, not to be read
/// at runtime (the real request type is `crate::types::ChatCompletionRequest`).
#[derive(ToSchema)]
#[allow(dead_code)]
pub struct ChatCompletionRequestCore {
    /// Model alias to route to — one of the aliases returned by `GET /v1/models`.
    #[schema(example = "claude-sonnet")]
    pub model: String,
    /// Conversation messages in OpenAI format. Modeled here as opaque objects;
    /// see the upstream spec for the role/content/tool-call union.
    #[schema(value_type = Vec<Object>)]
    pub messages: Vec<serde_json::Value>,
    /// When `true`, the response is streamed as SSE chunks terminated by a
    /// final `data: [DONE]` sentinel.
    pub stream: Option<bool>,
}

/// Core of the Anthropic-format messages request. As with the OpenAI path, the
/// full body is forwarded upstream; only owned fields are modeled. See
/// <https://docs.anthropic.com/en/api/messages> for the exhaustive schema.
///
/// Schema-only (see `ChatCompletionRequestCore`): describes the request body,
/// not read at runtime.
#[derive(ToSchema)]
#[allow(dead_code)]
pub struct AnthropicMessagesRequestCore {
    /// Model alias to route to — one of the aliases from `GET /anthropic/v1/models`.
    #[schema(example = "claude-sonnet")]
    pub model: String,
    /// Upper bound on tokens to generate (Anthropic requires this field).
    #[schema(example = 1024)]
    pub max_tokens: u32,
    /// Conversation messages in Anthropic format. Modeled here as opaque
    /// objects; see the upstream spec for the content-block union.
    #[schema(value_type = Vec<Object>)]
    pub messages: Vec<serde_json::Value>,
    /// When `true`, the response is streamed as Anthropic SSE events.
    pub stream: Option<bool>,
}

/// Adds the two authentication schemes the gateway accepts to the spec's
/// components so annotated routes can reference them.
struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi
            .components
            .as_mut()
            .expect("components exist once schemas are registered");
        // OpenAI-format routes: `Authorization: Bearer <virtual key | JWT>`.
        components.add_security_scheme(
            "bearer_auth",
            SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
        );
        // Anthropic-native routes also accept `x-api-key: <virtual key>`.
        components.add_security_scheme(
            "api_key_auth",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::new("x-api-key"))),
        );
    }
}

/// Documentation-only stub for the Prometheus metrics endpoint. It has no
/// dedicated handler function (it's a closure over `gather()` in `server.rs`),
/// so its path item is described here. Public, unauthenticated; present only
/// when `telemetry.metrics.enabled` (default true), at `telemetry.metrics.path`
/// (default `/metrics`).
#[utoipa::path(
    get,
    path = "/metrics",
    tag = "Observability",
    responses(
        (status = 200, description = "Prometheus/OpenMetrics text exposition", content_type = "text/plain")
    )
)]
#[allow(dead_code)]
fn metrics_doc() {}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Ferrox Gateway API",
        description = "Stateless, OpenAI-compatible LLM gateway. Exposes OpenAI-format and \
                       Anthropic-native inference routes plus operational endpoints. The \
                       chat/messages request and response bodies mirror the upstream OpenAI / \
                       Anthropic wire formats — only the fields Ferrox owns are modeled here; \
                       refer to the upstream specs for the exhaustive schema.",
    ),
    paths(
        crate::handlers::chat::chat_completions,
        crate::handlers::models::list_models,
        crate::handlers::anthropic_messages::anthropic_messages,
        crate::handlers::anthropic_models::list_models_anthropic,
        crate::handlers::health::healthz,
        crate::handlers::health::readyz,
        metrics_doc,
    ),
    components(schemas(
        ErrorResponse,
        ErrorDetail,
        ChatCompletionRequestCore,
        AnthropicMessagesRequestCore,
        crate::types::ModelsResponse,
        crate::types::ModelObject,
        crate::anthropic_types::AnthropicModelsResponse,
        crate::anthropic_types::AnthropicModelObject,
    )),
    modifiers(&SecurityAddon),
    tags(
        (name = "OpenAI", description = "OpenAI-compatible inference routes (Authorization: Bearer)"),
        (name = "Anthropic", description = "Anthropic-native inference routes (x-api-key or Bearer)"),
        (name = "Observability", description = "Health and metrics (public, unauthenticated)"),
    )
)]
pub struct ApiDoc;

/// The generated OpenAPI document as a pretty-printed JSON string, built once.
/// The `info.version` is stamped from the crate version at build time (utoipa's
/// `info(version=...)` attribute only accepts a string literal, so it's set
/// here instead).
static OPENAPI_JSON: Lazy<String> = Lazy::new(|| {
    let mut doc = ApiDoc::openapi();
    doc.info.version = env!("CARGO_PKG_VERSION").to_string();
    doc.to_pretty_json()
        .expect("OpenAPI document serializes to JSON")
});

/// Cached OpenAPI JSON served by the `/api-schema` and `/openapi.json` routes.
pub fn openapi_json() -> &'static str {
    OPENAPI_JSON.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Absolute path to the committed snapshot, read at runtime (not via
    /// `include_str!`) so the crate still compiles before the file is first
    /// generated by `regenerate_committed_snapshot`.
    const SNAPSHOT_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/openapi.json");

    #[test]
    fn document_is_valid_openapi_3x() {
        let v: serde_json::Value = serde_json::from_str(openapi_json()).unwrap();
        assert!(
            v["openapi"].as_str().unwrap_or_default().starts_with("3."),
            "openapi version must be 3.x, got {:?}",
            v["openapi"]
        );
        assert!(v["info"]["title"].is_string());
        assert!(v["paths"].is_object());
    }

    #[test]
    fn all_gateway_routes_are_enumerated() {
        let v: serde_json::Value = serde_json::from_str(openapi_json()).unwrap();
        let paths = &v["paths"];
        for p in [
            "/v1/chat/completions",
            "/v1/models",
            "/anthropic/v1/messages",
            "/anthropic/v1/models",
            "/healthz",
            "/readyz",
            "/metrics",
        ] {
            assert!(paths.get(p).is_some(), "route {p} missing from schema");
        }
    }

    #[test]
    fn owned_shapes_and_security_are_modeled() {
        let v: serde_json::Value = serde_json::from_str(openapi_json()).unwrap();
        let schemas = &v["components"]["schemas"];
        for s in [
            "ErrorResponse",
            "ErrorDetail",
            "ModelsResponse",
            "ModelObject",
            "AnthropicModelsResponse",
            "AnthropicModelObject",
        ] {
            assert!(schemas.get(s).is_some(), "schema {s} missing");
        }
        // Error envelope shape: { error: { message, type, code } }.
        let props = &schemas["ErrorDetail"]["properties"];
        assert!(props.get("message").is_some());
        assert!(props.get("type").is_some(), "error `type` field missing");
        assert!(props.get("code").is_some());
        // Both auth schemes present.
        let sec = &v["components"]["securitySchemes"];
        assert!(sec["bearer_auth"].is_object());
        assert!(sec["api_key_auth"].is_object());
    }

    /// Drift guard: the committed `ferrox/openapi.json` must match the spec
    /// generated from the code. Regenerate with:
    /// `cargo test -p ferrox openapi::tests::regenerate_committed_snapshot -- --ignored`
    #[test]
    fn committed_snapshot_matches_generated() {
        let committed = std::fs::read_to_string(SNAPSHOT_PATH).unwrap_or_else(|e| {
            panic!("committed snapshot {SNAPSHOT_PATH} unreadable ({e}); generate it with the ignored `regenerate_committed_snapshot` test")
        });
        assert_eq!(
            openapi_json().trim(),
            committed.trim(),
            "ferrox/openapi.json is out of date — regenerate with the ignored \
             `regenerate_committed_snapshot` test and commit it"
        );
    }

    /// Writes the current spec to the committed snapshot. Ignored by default so
    /// it never runs in CI; invoke explicitly to (re)generate the file.
    #[test]
    #[ignore = "regeneration utility, run explicitly"]
    fn regenerate_committed_snapshot() {
        std::fs::write(SNAPSHOT_PATH, format!("{}\n", openapi_json())).unwrap();
    }
}
