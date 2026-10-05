use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use axum::response::sse::{Event, KeepAlive};
use axum::{
    extract::{Extension, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response, Sse},
    Json,
};
use ferrox_providers::responses_emitter::{
    strip_thinking_signature, strip_thinking_signature_chunk,
};
use futures::StreamExt;

use crate::budget_enforcer::BudgetReservation;
use crate::classifier::{skip_requested, ClassifierInput};
use crate::config::RetryConfig;
use crate::error::ProxyError;
use crate::handlers::finalize::{record_error_metrics, RequestFinalizer, Surface};
use crate::lb::{RoutePool, RouteTarget};
use crate::providers::{ProviderAdapter, ProviderStream};
use crate::retry::{execute_with_retry, should_failover};
use crate::state::AppState;
use crate::telemetry::metrics::FALLBACK_TOTAL;
use crate::types::{ChatCompletionRequest, ChatCompletionResponse, RequestContext};

#[utoipa::path(
    post,
    path = "/v1/chat/completions",
    tag = "OpenAI",
    security(("bearer_auth" = [])),
    request_body = ChatCompletionRequestCore,
    params(
        ("x-ferrox-classifier" = Option<String>, Header, nullable = false, description = "Set to `skip` \
            (case-insensitive) to skip classification for this request: a classified alias \
            is then served by its `fallback_alias` and the classifier is not called. Any \
            other value, and the header on a statically routed alias, has no effect."),
    ),
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
    headers: HeaderMap,
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

    let decision = state
        .resolver
        .resolve(&req.model, skip_requested(&headers), |max_input_chars| {
            ClassifierInput::from_chat(&req, max_input_chars)
        })
        .await?;
    decision.log_classification(&ctx.request_id);
    let pool = &decision.pool;
    let model_alias = decision.served_alias();

    tracing::info!(
        request_id = %ctx.request_id,
        key_name = %ctx.key_name,
        model_alias = %model_alias,
        streaming = req.is_streaming(),
        "Dispatching request"
    );

    let retry_config = &state.config.defaults.retry;

    if req.is_streaming() {
        match dispatch_stream(pool, &req, retry_config).await {
            Ok((stream, provider_name, model_id)) => {
                let finalizer = RequestFinalizer::new(
                    &state,
                    &ctx,
                    reservation.map(|Extension(r)| r),
                    model_alias.to_string(),
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
                record_error_metrics(model_alias, "", &e, start);
                Err(e)
            }
        }
    } else {
        match dispatch_non_stream(pool, &req, retry_config).await {
            Ok((mut resp, provider_name, model_id)) => {
                strip_thinking_signature(&mut resp);
                RequestFinalizer::new(
                    &state,
                    &ctx,
                    reservation.map(|Extension(r)| r),
                    model_alias.to_string(),
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
                record_error_metrics(model_alias, "", &e, start);
                Err(e)
            }
        }
    }
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
    // A permit dropped unsettled gives a claimed half-open probe slot back:
    // the non-failover error below, and this future being dropped mid-attempt.
    if let Some((target, permit)) = primary {
        let provider_name = target.provider.name().to_string();
        let model_id = target.model_id.clone();

        match attempt(target, retry_config, &pool.alias, &call).await {
            Ok(out) => {
                permit.success();
                return Ok((out, provider_name, model_id));
            }
            Err(e) if should_failover(&e) => {
                permit.failure();
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
        // `skip` before `try_acquire`: a target that is not attempted must
        // not claim its half-open breaker's probe slot.
        if let Some(reason) = skip(fallback) {
            skipped.get_or_insert(reason);
            continue;
        }
        let Some(permit) = fallback.try_acquire() else {
            continue;
        };
        let provider_name = fallback.provider.name().to_string();
        let model_id = fallback.model_id.clone();

        match attempt(fallback, retry_config, &pool.alias, &call).await {
            Ok(out) => {
                permit.success();
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
                permit.failure();
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
    use std::time::Duration;

    use super::*;
    use crate::lb::circuit_breaker::CircuitState;
    use crate::lb::test_support::{pool, trip, Reply};

    fn allowed(models: &[&str]) -> Vec<String> {
        models.iter().map(|s| s.to_string()).collect()
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

    async fn dispatch_chat(
        pool: &RoutePool,
        retry: &RetryConfig,
    ) -> Result<(ChatCompletionResponse, String, String), ProxyError> {
        let req: ChatCompletionRequest =
            serde_json::from_value(serde_json::json!({"model": "m", "messages": []})).unwrap();
        dispatch_non_stream(pool, &req, retry).await
    }

    #[tokio::test]
    async fn a_probe_ending_in_a_non_failover_error_releases_its_slot() {
        let (pool, config) = pool("chat-probe-400", "failover", &[Reply::BadRequest], &[]);
        let breaker = &pool.targets[0].circuit_breaker;
        trip(&pool.targets[0], true);

        let err = dispatch_chat(&pool, &config.defaults.retry)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProxyError::ProviderError { status: 400, .. }),
            "{err}"
        );

        // Neither a success nor a failure: still half-open, and the next
        // request may probe.
        assert_eq!(breaker.state(), CircuitState::HalfOpen);
        assert!(!breaker.probe_in_flight());
        assert!(pool.select_target(|_| true).is_some());
    }

    #[tokio::test]
    async fn dropping_dispatch_mid_probe_releases_the_primary_slot() {
        let (pool, config) = pool("chat-probe-drop", "round_robin", &[Reply::Hang], &[]);
        let breaker = &pool.targets[0].circuit_breaker;
        trip(&pool.targets[0], true);

        let mut request = Box::pin(dispatch_chat(&pool, &config.defaults.retry));
        let pending = tokio::time::timeout(Duration::from_millis(20), &mut request).await;
        assert!(pending.is_err(), "the upstream never answers");
        assert!(breaker.probe_in_flight(), "the attempt holds the probe");

        // The client disconnects.
        drop(request);
        assert_eq!(breaker.state(), CircuitState::HalfOpen);
        assert!(!breaker.probe_in_flight());
    }

    #[tokio::test]
    async fn dropping_dispatch_mid_probe_releases_a_fallback_slot() {
        let (pool, config) = pool(
            "chat-fallback-drop",
            "failover",
            &[Reply::Unavailable],
            &[Reply::Hang],
        );
        let breaker = &pool.fallbacks[0].circuit_breaker;
        trip(&pool.fallbacks[0], true);

        let mut request = Box::pin(dispatch_chat(&pool, &config.defaults.retry));
        let pending = tokio::time::timeout(Duration::from_millis(20), &mut request).await;
        assert!(pending.is_err(), "the fallback never answers");
        assert!(breaker.probe_in_flight(), "the attempt holds the probe");

        drop(request);
        assert_eq!(breaker.state(), CircuitState::HalfOpen);
        assert!(!breaker.probe_in_flight());
    }
}
