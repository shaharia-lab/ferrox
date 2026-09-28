use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use axum::response::sse::{Event, KeepAlive};
use axum::{
    extract::{Extension, State},
    http::StatusCode,
    response::{IntoResponse, Response, Sse},
    Json,
};
use ferrox_providers::responses_emitter::RESPONSES_THINKING_SIGNATURE;
use futures::StreamExt;

use crate::budget_enforcer::BudgetReservation;
use crate::config::RetryConfig;
use crate::error::ProxyError;
use crate::handlers::finalize::{record_error_metrics, RequestFinalizer, Surface};
use crate::lb::{RoutePool, RouteTarget};
use crate::providers::{ProviderAdapter, ProviderStream};
use crate::retry::{execute_with_retry, should_failover};
use crate::state::AppState;
use crate::telemetry::metrics::FALLBACK_TOTAL;
use crate::types::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, RequestContext,
};

#[utoipa::path(
    post,
    path = "/v1/chat/completions",
    tag = "OpenAI",
    security(("bearer_auth" = [])),
    request_body = ChatCompletionRequestCore,
    responses(
        (status = 200, description = "Chat completion. JSON body (mirrors the OpenAI Chat \
            Completions response), or an SSE stream of `chat.completion.chunk` events \
            terminated by `data: [DONE]` when `stream=true`."),
        (status = 401, description = "Missing or invalid credentials", body = ErrorResponse),
        (status = 403, description = "Model not permitted for this key", body = ErrorResponse),
        (status = 404, description = "Unknown model alias", body = ErrorResponse),
        (status = 429, description = "Rate limited or budget exceeded", body = ErrorResponse),
        (status = 502, description = "Upstream provider error", body = ErrorResponse),
    )
)]
pub async fn chat_completions(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    reservation: Option<Extension<BudgetReservation>>,
    Json(req): Json<ChatCompletionRequest>,
) -> Result<Response, ProxyError> {
    let start = Instant::now();

    // Model access guard
    if !is_model_allowed(&req.model, &ctx.allowed_models) {
        return Err(ProxyError::Forbidden(format!(
            "Key '{}' is not authorized to use model '{}'",
            ctx.key_name, req.model
        )));
    }

    let pool = state.router.resolve(&req.model)?;

    tracing::info!(
        request_id = %ctx.request_id,
        key_name = %ctx.key_name,
        model_alias = %req.model,
        streaming = req.is_streaming(),
        "Dispatching request"
    );

    let retry_config = &state.config.defaults.retry;

    if req.is_streaming() {
        match dispatch_stream(&pool, &req, retry_config).await {
            Ok((stream, provider_name, model_id)) => {
                let finalizer = RequestFinalizer::new(
                    &state,
                    &ctx,
                    reservation.map(|Extension(r)| r),
                    req.model.clone(),
                    provider_name,
                    model_id,
                    start,
                    Surface::OpenAi,
                );
                // The wrapper finishes its accounting before it ends, so the
                // chained `[DONE]` always follows the recorded usage.
                let sse_stream = finalizer
                    .wrap_stream(stream)
                    .filter_map(|chunk_result| {
                        let event = match chunk_result {
                            Ok(chunk) => strip_thinking_signature_chunk(chunk).map(|chunk| {
                                let data = serde_json::to_string(&chunk).unwrap_or_default();
                                Ok(Event::default().data(data))
                            }),
                            Err(e) => Some(Err(e)),
                        };
                        std::future::ready(event)
                    })
                    .chain(futures::stream::once(async {
                        Ok::<Event, ProxyError>(Event::default().data("[DONE]"))
                    }));

                Ok(Sse::new(sse_stream)
                    .keep_alive(KeepAlive::default())
                    .into_response())
            }
            Err(e) => {
                record_error_metrics(&req.model, "", &e, start);
                Err(e)
            }
        }
    } else {
        match dispatch_non_stream(&pool, &req, retry_config).await {
            Ok((mut resp, provider_name, model_id)) => {
                strip_thinking_signature(&mut resp);
                RequestFinalizer::new(
                    &state,
                    &ctx,
                    reservation.map(|Extension(r)| r),
                    req.model.clone(),
                    provider_name,
                    model_id,
                    start,
                    Surface::OpenAi,
                )
                .finish(resp.usage.as_ref())
                .await;
                Ok(Json(resp).into_response())
            }
            Err(e) => {
                record_error_metrics(&req.model, "", &e, start);
                Err(e)
            }
        }
    }
}

// ── Private carriers ──────────────────────────────────────────────────────────

/// Remove the Anthropic thinking signature an adapter left on the message for
/// the Responses encoder. Chat Completions has no field for it, and
/// `_`-prefixed keys are gateway-private.
fn strip_thinking_signature(resp: &mut ChatCompletionResponse) {
    for choice in &mut resp.choices {
        choice.message.extra.remove(RESPONSES_THINKING_SIGNATURE);
    }
}

/// Streaming counterpart of [`strip_thinking_signature`]. A chunk that only
/// carried the signature is dropped (`None`) instead of sent empty.
fn strip_thinking_signature_chunk(mut chunk: ChatCompletionChunk) -> Option<ChatCompletionChunk> {
    let mut stripped = false;
    for choice in &mut chunk.choices {
        stripped |= choice.extra.remove(RESPONSES_THINKING_SIGNATURE).is_some();
    }
    let empty = chunk.usage.is_none()
        && chunk.choices.iter().all(|c| {
            c.extra.is_empty()
                && c.finish_reason.is_none()
                && c.delta.role.is_none()
                && c.delta.content.is_none()
                && c.delta.tool_calls.is_none()
                && c.delta.reasoning_content.is_none()
        });
    (!(stripped && empty)).then_some(chunk)
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

/// Returns `(response, provider_name, model_id)` on success.
pub(crate) async fn dispatch_non_stream(
    pool: &RoutePool,
    req: &ChatCompletionRequest,
    retry_config: &RetryConfig,
) -> Result<(ChatCompletionResponse, String, String), ProxyError> {
    dispatch(
        pool,
        retry_config,
        false,
        |_| None,
        |provider, model_id| async move { provider.chat(req, &model_id).await },
    )
    .await
}

/// Returns `(stream, provider_name, model_id)` on success.
pub(crate) async fn dispatch_stream(
    pool: &RoutePool,
    req: &ChatCompletionRequest,
    retry_config: &RetryConfig,
) -> Result<(ProviderStream, String, String), ProxyError> {
    dispatch(
        pool,
        retry_config,
        true,
        |_| None,
        |provider, model_id| async move { provider.chat_stream(req, &model_id).await },
    )
    .await
}

/// Serve one request from `pool`: the selected primary target, then — when
/// its error warrants failover — each available fallback, every attempt
/// wrapped in the retry policy and recorded on the target's circuit breaker.
/// `call` makes one attempt against a provider and upstream model id, so
/// each surface chooses per target what that attempt is. `streaming` only
/// selects the log messages.
///
/// `skip` returns why a target cannot serve this request at all (a
/// Responses request using native-only features, on a translate-only
/// provider). Such a target is never selected as the primary, and is passed
/// over in the fallback chain, without an attempt and without touching its
/// circuit breaker (not even claiming a half-open probe). When every target
/// is skipped, the first such reason is the error; otherwise a request no
/// target served is a 502.
///
/// Returns `(output, provider_name, model_id)` on success.
pub(crate) async fn dispatch<T, F, Fut>(
    pool: &RoutePool,
    retry_config: &RetryConfig,
    streaming: bool,
    skip: impl Fn(&RouteTarget) -> Option<ProxyError>,
    call: F,
) -> Result<(T, String, String), ProxyError>
where
    F: Fn(Arc<dyn ProviderAdapter>, String) -> Fut,
    Fut: Future<Output = Result<T, ProxyError>>,
{
    let log = if streaming {
        &STREAM_LOG
    } else {
        &NON_STREAM_LOG
    };

    // Try primary targets: the strategy picks among the available ones this
    // request can go to.
    let primary = pool.select_target(|t| skip(t).is_none());
    let mut skipped = match primary {
        Some(_) => None,
        None => pool.targets.iter().find_map(&skip),
    };
    if let Some(target) = primary {
        let provider_name = target.provider.name().to_string();
        let model_id = target.model_id.clone();

        match attempt(target, retry_config, &pool.alias, &call).await {
            Ok(out) => {
                target.circuit_breaker.record_success();
                return Ok((out, provider_name, model_id));
            }
            Err(e) if should_failover(&e) => {
                target.circuit_breaker.record_failure();
                tracing::warn!(
                    provider = %provider_name,
                    model_id = %model_id,
                    error = %e,
                    "{}",
                    log.primary_failed
                );
            }
            Err(e) => return Err(e),
        }
    }

    // Fallback chain
    for fallback in &pool.fallbacks {
        // `skip` before `is_available`: the latter claims a half-open
        // breaker's probe slot, which only an attempt gives back.
        if let Some(reason) = skip(fallback) {
            skipped.get_or_insert(reason);
            continue;
        }
        if !fallback.is_available() {
            continue;
        }
        let provider_name = fallback.provider.name().to_string();
        let model_id = fallback.model_id.clone();

        match attempt(fallback, retry_config, &pool.alias, &call).await {
            Ok(out) => {
                fallback.circuit_breaker.record_success();
                FALLBACK_TOTAL
                    .with_label_values(&[pool.alias.as_str(), "", provider_name.as_str()])
                    .inc();
                tracing::info!(
                    provider = %provider_name,
                    model_id = %model_id,
                    "{}",
                    log.served_by_fallback
                );
                return Ok((out, provider_name, model_id));
            }
            Err(e) => {
                fallback.circuit_breaker.record_failure();
                tracing::warn!(provider = %provider_name, error = %e, "{}", log.fallback_failed);
            }
        }
    }

    // The skip reason is the answer only when no target could ever serve the
    // request. One that could but is down (breaker open) or failed makes this
    // a retryable 502, not a client error. Only reached with a reason when
    // something was skipped, so the chat path never evaluates this.
    match skipped {
        Some(reason)
            if pool
                .targets
                .iter()
                .chain(&pool.fallbacks)
                .all(|t| skip(t).is_some()) =>
        {
            Err(reason)
        }
        _ => Err(ProxyError::ProviderError {
            provider: pool.alias.clone(),
            status: StatusCode::BAD_GATEWAY.as_u16(),
            message: "All targets and fallbacks failed".to_string(),
        }),
    }
}

/// The failover log messages, unchanged per mode so existing log queries keep
/// matching.
struct DispatchLog {
    primary_failed: &'static str,
    served_by_fallback: &'static str,
    fallback_failed: &'static str,
}

const NON_STREAM_LOG: DispatchLog = DispatchLog {
    primary_failed: "Primary target failed — trying fallback chain",
    served_by_fallback: "Request served by fallback",
    fallback_failed: "Fallback failed",
};

const STREAM_LOG: DispatchLog = DispatchLog {
    primary_failed: "Primary streaming target failed — trying fallback",
    served_by_fallback: "Streaming request served by fallback",
    fallback_failed: "Streaming fallback failed",
};

async fn attempt<T, F, Fut>(
    target: &RouteTarget,
    retry_config: &RetryConfig,
    model_alias: &str,
    call: &F,
) -> Result<T, ProxyError>
where
    F: Fn(Arc<dyn ProviderAdapter>, String) -> Fut,
    Fut: Future<Output = Result<T, ProxyError>>,
{
    let provider_name = target.provider.name().to_string();

    execute_with_retry(retry_config, &provider_name, model_alias, || {
        call(target.provider.clone(), target.model_id.clone())
    })
    .await
}

pub fn is_model_allowed(model: &str, allowed: &[String]) -> bool {
    allowed.iter().any(|a| a == "*" || a == model)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(models: &[&str]) -> Vec<String> {
        models.iter().map(|s| s.to_string()).collect()
    }

    fn chunk_with(delta: serde_json::Value, extra: serde_json::Value) -> ChatCompletionChunk {
        let mut choice = serde_json::json!({"index": 0, "delta": delta, "finish_reason": null});
        for (k, v) in extra.as_object().unwrap() {
            choice[k] = v.clone();
        }
        serde_json::from_value(
            serde_json::json!({"id": "c", "object": "chat.completion.chunk",
            "created": 0, "model": "m", "choices": [choice]}),
        )
        .unwrap()
    }

    #[test]
    fn signature_only_chunk_is_dropped_from_the_chat_stream() {
        let chunk = chunk_with(
            serde_json::json!({}),
            serde_json::json!({RESPONSES_THINKING_SIGNATURE: "sig"}),
        );
        assert!(strip_thinking_signature_chunk(chunk).is_none());
    }

    #[test]
    fn other_chunks_pass_without_the_signature() {
        let chunk = chunk_with(
            serde_json::json!({"reasoning_content": "t"}),
            serde_json::json!({RESPONSES_THINKING_SIGNATURE: "sig", "logprobs": null}),
        );
        let out = strip_thinking_signature_chunk(chunk).expect("kept");
        let json = serde_json::to_value(&out).unwrap();
        assert!(json["choices"][0]
            .get(RESPONSES_THINKING_SIGNATURE)
            .is_none());
        assert_eq!(json["choices"][0]["delta"]["reasoning_content"], "t");

        // A chunk that never had a signature is untouched, even when empty.
        let empty = chunk_with(serde_json::json!({}), serde_json::json!({}));
        assert!(strip_thinking_signature_chunk(empty).is_some());
    }

    #[test]
    fn signature_is_removed_from_the_chat_response() {
        let mut resp: ChatCompletionResponse = serde_json::from_value(serde_json::json!({
            "id": "c", "object": "chat.completion", "created": 0, "model": "m",
            "choices": [{"index": 0, "finish_reason": "stop", "message": {
                "role": "assistant", "content": "hi", "reasoning_content": "t",
                RESPONSES_THINKING_SIGNATURE: "sig"}}]}))
        .unwrap();
        assert!(resp.choices[0]
            .message
            .extra
            .contains_key(RESPONSES_THINKING_SIGNATURE));
        strip_thinking_signature(&mut resp);
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json["choices"][0]["message"]
            .get(RESPONSES_THINKING_SIGNATURE)
            .is_none());
        assert_eq!(json["choices"][0]["message"]["content"], "hi");
    }

    #[test]
    fn wildcard_allows_any_model() {
        assert!(is_model_allowed("gpt-4", &allowed(&["*"])));
        assert!(is_model_allowed("claude-3", &allowed(&["*"])));
    }

    #[test]
    fn exact_match_allows_specific_model() {
        assert!(is_model_allowed("gpt-4", &allowed(&["gpt-4", "claude-3"])));
    }

    #[test]
    fn no_match_denies_model() {
        assert!(!is_model_allowed("gpt-5", &allowed(&["gpt-4", "claude-3"])));
    }

    #[test]
    fn empty_allowed_list_denies_all() {
        assert!(!is_model_allowed("gpt-4", &[]));
    }

    #[test]
    fn partial_name_is_not_a_match() {
        assert!(!is_model_allowed("gpt", &allowed(&["gpt-4"])));
    }

    #[test]
    fn wildcard_mixed_with_others_still_works() {
        assert!(is_model_allowed(
            "anything",
            &allowed(&["gpt-4", "*", "claude-3"])
        ));
    }
}
