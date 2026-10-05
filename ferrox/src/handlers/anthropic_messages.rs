use std::time::Instant;

use axum::{
    body::Bytes,
    extract::{Extension, State},
    http::StatusCode,
    response::{sse::KeepAlive, IntoResponse, Response, Sse},
    Json,
};
use futures::StreamExt as _;
use uuid::Uuid;

use crate::anthropic_types::{
    openai_stream_to_anthropic_sse, to_anthropic_response, to_chat_completion_request,
    AnthropicMessagesRequest,
};
use crate::budget_enforcer::BudgetReservation;
use crate::classifier::{skip_requested, ClassifierInput};
use crate::error::ProxyError;
use crate::handlers::chat::{dispatch_non_stream, dispatch_stream, is_model_allowed};
use crate::handlers::finalize::{
    record_error_count, record_error_metrics, RequestFinalizer, Surface,
};
use crate::state::AppState;
use crate::types::RequestContext;

// ── Anthropic-format error responses ─────────────────────────────────────────

/// Map a `ProxyError` to an Anthropic API error response.
///
/// The mapping itself lives in `ferrox-providers` so that any consumer exposing
/// an Anthropic-native surface on the crate emits byte-identical errors; this is
/// only the axum wrapper around it.
fn proxy_error_to_anthropic_response(e: &ProxyError) -> Response {
    let (status, body) = ferrox_providers::error::anthropic_error_body(e);
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    (status, Json(body)).into_response()
}

#[utoipa::path(
    post,
    path = "/anthropic/v1/messages",
    tag = "Anthropic",
    security(("api_key_auth" = []), ("bearer_auth" = [])),
    request_body = AnthropicMessagesRequestCore,
    params(
        ("x-ferrox-classifier" = Option<String>, Header, nullable = false, description = "Set to `skip` \
            (case-insensitive) to skip classification for this request: a classified alias \
            is then served by its `fallback_alias` and the classifier is not called. Any \
            other value, and the header on a statically routed alias, has no effect."),
    ),
    responses(
        (status = 200, description = "Message response. JSON body (mirrors the Anthropic \
            Messages response), or an Anthropic SSE event stream when `stream=true`.",
            headers(("x-ferrox-routed-model" = String, description = "The statically routed \
                alias that served the request. Only present when `model` is a classified alias."))),
        (status = 401, description = "Missing or invalid credentials", body = ErrorResponse),
        (status = 403, description = "Model not permitted for this key", body = ErrorResponse),
        (status = 404, description = "Unknown model alias", body = ErrorResponse),
        (status = 429, description = "Rate limited or budget exceeded", body = ErrorResponse),
        (status = 502, description = "Upstream provider error", body = ErrorResponse),
    )
)]
pub async fn anthropic_messages(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    reservation: Option<Extension<BudgetReservation>>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Result<Response, ProxyError> {
    let start = Instant::now();

    // Parse the raw body once, keeping the original JSON value so provider
    // adapters can forward it verbatim (preserving every Anthropic-specific
    // field the client sent).
    let raw: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return Ok(proxy_error_to_anthropic_response(
                &ProxyError::SerializationError(e),
            ));
        }
    };
    let req: AnthropicMessagesRequest = match serde_json::from_value(raw.clone()) {
        Ok(r) => r,
        Err(e) => {
            return Ok(proxy_error_to_anthropic_response(
                &ProxyError::SerializationError(e),
            ));
        }
    };

    if !is_model_allowed(&req.model, &ctx.allowed_models) {
        return Ok(proxy_error_to_anthropic_response(&ProxyError::Forbidden(
            format!(
                "Key '{}' is not authorized to use model '{}'",
                ctx.key_name, req.model
            ),
        )));
    }

    let is_streaming = req.is_streaming();
    let retry_config = &state.config.defaults.retry;

    // Forward the `anthropic-beta` header — the only user-controllable Anthropic
    // header documented in the official API reference.  Merge any `betas` array
    // from the request body with the header value into a single comma-separated
    // string (the Anthropic SDK sometimes sends betas via the body field).
    let beta_header_value: Option<String> = {
        let mut betas: Vec<String> = Vec::new();
        if let Some(v) = headers.get("anthropic-beta").and_then(|v| v.to_str().ok()) {
            betas.push(v.to_string());
        }
        if let Some(arr) = raw.get("betas").and_then(|v| v.as_array()) {
            for b in arr {
                if let Some(s) = b.as_str() {
                    betas.push(s.to_string());
                }
            }
        }
        if betas.is_empty() {
            None
        } else {
            Some(betas.join(","))
        }
    };

    let mut internal_req = to_chat_completion_request(req);

    // Attach the original body so the Anthropic provider can forward it verbatim.
    internal_req.raw_anthropic_body = Some(raw);

    if let Some(beta) = beta_header_value {
        internal_req
            .extra_headers
            .insert("anthropic-beta".to_string(), beta);
    }

    // Resolved on the chat form of the request, so a classified alias sees
    // the same input here as on `/v1/chat/completions`.
    let decision = match state
        .resolver
        .resolve(
            &internal_req.model,
            skip_requested(&headers),
            |max_input_chars| ClassifierInput::from_chat(&internal_req, max_input_chars),
        )
        .await
    {
        Ok(decision) => decision,
        Err(e) => return Ok(proxy_error_to_anthropic_response(&e)),
    };
    decision.log_classification(&ctx.request_id);
    let pool = &decision.pool;
    let model_alias = decision.served_alias();

    tracing::info!(
        request_id = %ctx.request_id,
        key_name   = %ctx.key_name,
        model_alias = %model_alias,
        streaming  = is_streaming,
        "Dispatching Anthropic-format request"
    );

    if is_streaming {
        let msg_id = format!("msg_{}", Uuid::new_v4().simple());

        match dispatch_stream(pool, &internal_req, retry_config).await {
            Ok((stream, provider_name, model_id)) => {
                let finalizer = RequestFinalizer::new(
                    &state,
                    &ctx,
                    reservation.map(|Extension(r)| r),
                    model_alias.to_string(),
                    provider_name,
                    model_id,
                    start,
                    Surface::Anthropic,
                )
                .with_routing(decision.routing_record());
                // Meter the OpenAI-format stream before converting it to
                // Anthropic SSE; the converter drains it before emitting
                // `message_stop`, so usage is recorded ahead of that frame.
                let anthropic_stream = openai_stream_to_anthropic_sse(
                    internal_req.model.clone(),
                    msg_id,
                    finalizer.wrap_stream(stream).boxed(),
                );

                // Return a silent SSE comment; Anthropic SDK ignores it.
                let sse_stream = anthropic_stream.chain(futures::stream::once(async {
                    Ok::<_, ProxyError>(axum::response::sse::Event::default().comment("done"))
                }));

                Ok(decision.with_routed_model_header(
                    Sse::new(sse_stream)
                        .keep_alive(KeepAlive::default())
                        .into_response(),
                ))
            }
            Err(e) => {
                record_error_count(model_alias, "", &e);
                Ok(proxy_error_to_anthropic_response(&e))
            }
        }
    } else {
        match dispatch_non_stream(pool, &internal_req, retry_config).await {
            Ok((resp, provider_name, model_id)) => {
                RequestFinalizer::new(
                    &state,
                    &ctx,
                    reservation.map(|Extension(r)| r),
                    model_alias.to_string(),
                    provider_name,
                    model_id,
                    start,
                    Surface::Anthropic,
                )
                .with_routing(decision.routing_record())
                .finish(resp.usage.as_ref())
                .await;
                Ok(decision
                    .with_routed_model_header(Json(to_anthropic_response(resp)).into_response()))
            }
            Err(e) => {
                record_error_metrics(model_alias, "", &e, start);
                Ok(proxy_error_to_anthropic_response(&e))
            }
        }
    }
}
