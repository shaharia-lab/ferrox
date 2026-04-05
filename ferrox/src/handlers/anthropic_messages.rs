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
use crate::error::ProxyError;
use crate::handlers::chat::{dispatch_non_stream, dispatch_stream, is_model_allowed};
use crate::state::AppState;
use crate::telemetry::metrics::{
    self, ACTIVE_STREAMS, ERRORS_TOTAL, REQUESTS_TOTAL, REQUEST_DURATION_SECONDS,
};
use crate::types::RequestContext;

// ── Anthropic-format error responses ─────────────────────────────────────────

/// Map a `ProxyError` to an Anthropic API error response.
///
/// The Anthropic SDK expects errors in the format:
/// ```json
/// { "type": "error", "error": { "type": "<error_type>", "message": "..." } }
/// ```
fn proxy_error_to_anthropic_response(e: &ProxyError) -> Response {
    let (status, error_type, message) = match e {
        ProxyError::Unauthorized(msg) => (
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            msg.clone(),
        ),
        ProxyError::Forbidden(msg) => (StatusCode::FORBIDDEN, "permission_error", msg.clone()),
        ProxyError::ModelNotFound(msg) => (StatusCode::NOT_FOUND, "not_found_error", msg.clone()),
        ProxyError::RateLimited(msg) => (
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            msg.clone(),
        ),
        ProxyError::CircuitOpen(msg) => {
            // 529 is Anthropic's "overloaded" status; use BAD_GATEWAY as the closest standard code.
            (StatusCode::BAD_GATEWAY, "overloaded_error", msg.clone())
        }
        ProxyError::ProviderError {
            status, message, ..
        } => {
            let http_status = StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY);
            (http_status, "api_error", message.clone())
        }
        ProxyError::UpstreamTimeout(msg) => (StatusCode::GATEWAY_TIMEOUT, "api_error", msg.clone()),
        ProxyError::StreamError(msg) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "api_error", msg.clone())
        }
        other => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            other.to_string(),
        ),
    };

    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": error_type,
            "message": message
        }
    });

    (status, Json(body)).into_response()
}

pub async fn anthropic_messages(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
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
    let model_alias = req.model.clone();
    let pool = match state.router.resolve(&req.model) {
        Ok(p) => p,
        Err(e) => return Ok(proxy_error_to_anthropic_response(&e)),
    };
    let retry_config = &state.config.defaults.retry;

    tracing::info!(
        request_id = %ctx.request_id,
        key_name   = %ctx.key_name,
        model_alias = %model_alias,
        streaming  = is_streaming,
        "Dispatching Anthropic-format request"
    );

    let mut internal_req = to_chat_completion_request(req);

    // Attach the original body so the Anthropic provider can forward it verbatim.
    internal_req.raw_anthropic_body = Some(raw);

    // Forward the anthropic-beta header so the provider includes it upstream.
    if let Some(beta) = headers.get("anthropic-beta").and_then(|v| v.to_str().ok()) {
        internal_req
            .extra_headers
            .insert("anthropic-beta".to_string(), beta.to_string());
    }

    if is_streaming {
        let msg_id = format!("msg_{}", Uuid::new_v4().simple());
        let result = dispatch_stream(&pool, &internal_req, retry_config).await;

        match result {
            Ok((stream, provider_name, model_id)) => {
                let alias = model_alias.clone();

                ACTIVE_STREAMS
                    .with_label_values(&[provider_name.as_str(), alias.as_str()])
                    .inc();

                let p2 = provider_name.clone();
                let a2 = alias.clone();
                let k2 = ctx.key_name.clone();
                let m2 = model_id.clone();

                let anthropic_stream =
                    openai_stream_to_anthropic_sse(internal_req.model.clone(), msg_id, stream);

                // Chain a finalizer that records metrics and decrements the
                // active-streams counter once the client has consumed all events.
                let sse_stream = anthropic_stream.chain(futures::stream::once(async move {
                    let latency = start.elapsed().as_secs_f64();
                    REQUESTS_TOTAL
                        .with_label_values(&[
                            p2.as_str(),
                            a2.as_str(),
                            m2.as_str(),
                            "200",
                            k2.as_str(),
                        ])
                        .inc();
                    REQUEST_DURATION_SECONDS
                        .with_label_values(&[p2.as_str(), a2.as_str(), "200"])
                        .observe(latency);
                    ACTIVE_STREAMS
                        .with_label_values(&[p2.as_str(), a2.as_str()])
                        .dec();
                    tracing::info!(
                        model_alias = %a2,
                        provider   = %p2,
                        model_id   = %m2,
                        streaming  = true,
                        status     = 200,
                        latency_ms = (latency * 1000.0) as u64,
                        "anthropic_request_completed"
                    );
                    // Return a silent SSE comment; Anthropic SDK ignores it.
                    Ok::<_, ProxyError>(axum::response::sse::Event::default().comment("done"))
                }));

                Ok(Sse::new(sse_stream)
                    .keep_alive(KeepAlive::default())
                    .into_response())
            }
            Err(e) => {
                REQUESTS_TOTAL
                    .with_label_values(&[
                        "",
                        &model_alias,
                        "",
                        &http_status_for_error(&e).to_string(),
                        "",
                    ])
                    .inc();
                ERRORS_TOTAL
                    .with_label_values(&["", error_type_label(&e)])
                    .inc();
                Ok(proxy_error_to_anthropic_response(&e))
            }
        }
    } else {
        let result = dispatch_non_stream(&pool, &internal_req, retry_config).await;
        let latency = start.elapsed().as_secs_f64();

        match result {
            Ok((resp, provider_name, model_id)) => {
                if let Some(usage) = &resp.usage {
                    metrics::record_tokens(
                        &provider_name,
                        &model_alias,
                        usage.prompt_tokens,
                        usage.completion_tokens,
                    );
                }
                REQUESTS_TOTAL
                    .with_label_values(&[
                        provider_name.as_str(),
                        model_alias.as_str(),
                        model_id.as_str(),
                        "200",
                        ctx.key_name.as_str(),
                    ])
                    .inc();
                REQUEST_DURATION_SECONDS
                    .with_label_values(&[provider_name.as_str(), model_alias.as_str(), "200"])
                    .observe(latency);

                tracing::info!(
                    request_id = %ctx.request_id,
                    key_name   = %ctx.key_name,
                    model_alias = %model_alias,
                    provider   = %provider_name,
                    model_id   = %model_id,
                    streaming  = false,
                    status     = 200,
                    latency_ms = (latency * 1000.0) as u64,
                    "anthropic_request_completed"
                );

                Ok(Json(to_anthropic_response(resp)).into_response())
            }
            Err(e) => {
                REQUESTS_TOTAL
                    .with_label_values(&[
                        "",
                        &model_alias,
                        "",
                        &http_status_for_error(&e).to_string(),
                        "",
                    ])
                    .inc();
                ERRORS_TOTAL
                    .with_label_values(&["", error_type_label(&e)])
                    .inc();
                REQUEST_DURATION_SECONDS
                    .with_label_values(&["", &model_alias, &http_status_for_error(&e).to_string()])
                    .observe(latency);
                Ok(proxy_error_to_anthropic_response(&e))
            }
        }
    }
}

fn http_status_for_error(e: &ProxyError) -> u16 {
    match e {
        ProxyError::Unauthorized(_) => 401,
        ProxyError::Forbidden(_) => 403,
        ProxyError::ModelNotFound(_) => 404,
        ProxyError::RateLimited(_) => 429,
        ProxyError::CircuitOpen(_) | ProxyError::ProviderError { .. } => 502,
        ProxyError::UpstreamTimeout(_) => 504,
        _ => 500,
    }
}

fn error_type_label(e: &ProxyError) -> &'static str {
    match e {
        ProxyError::Unauthorized(_) => "unauthorized",
        ProxyError::Forbidden(_) => "forbidden",
        ProxyError::ModelNotFound(_) => "model_not_found",
        ProxyError::RateLimited(_) => "rate_limited",
        ProxyError::CircuitOpen(_) => "circuit_open",
        ProxyError::ProviderError { .. } => "provider_error",
        ProxyError::UpstreamTimeout(_) => "upstream_timeout",
        ProxyError::StreamError(_) => "stream_error",
        ProxyError::HttpClientError(_) => "http_client_error",
        ProxyError::AwsError(_) => "aws_error",
        _ => "internal",
    }
}
