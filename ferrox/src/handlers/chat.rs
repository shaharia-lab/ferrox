use std::time::Instant;

use axum::response::sse::{Event, KeepAlive};
use axum::{
    extract::{Extension, State},
    http::StatusCode,
    response::{IntoResponse, Response, Sse},
    Json,
};
use futures::StreamExt;

use crate::config::RetryConfig;
use crate::error::ProxyError;
use crate::lb::{RoutePool, RouteTarget};
use crate::providers::ProviderStream;
use crate::retry::{execute_with_retry, is_retryable};
use crate::state::AppState;
use crate::telemetry::metrics::{
    self, ACTIVE_STREAMS, ERRORS_TOTAL, FALLBACK_TOTAL, REQUESTS_TOTAL, REQUEST_DURATION_SECONDS,
};
use crate::types::{ChatCompletionRequest, ChatCompletionResponse, RequestContext};
use crate::usage_writer::UsageEvent;

pub async fn chat_completions(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
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
        let result = dispatch_stream(&pool, &req, retry_config).await;
        match result {
            Ok((stream, provider_name, model_id)) => {
                let alias = req.model.clone();
                let key_name = ctx.key_name.clone();

                ACTIVE_STREAMS
                    .with_label_values(&[provider_name.as_str(), alias.as_str()])
                    .inc();

                // Clone all labels needed by the two closures (map + chain)
                let p1 = provider_name.clone();
                let a1 = alias.clone();
                let k1 = key_name.clone();
                let usage_writer = state.usage_writer.clone();
                let budget_enforcer = state.budget_enforcer.clone();
                let stream_client_id = ctx.client_id;
                let stream_budget_period = ctx.budget_period.clone();
                let stream_budget_reserved = ctx.budget_reserved_tokens;
                let stream_request_id = ctx.request_id.clone();
                let stream_model = alias.clone();
                let stream_provider = provider_name.clone();
                let accumulated_prompt = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
                let accumulated_completion =
                    std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
                let acc_p = accumulated_prompt.clone();
                let acc_c = accumulated_completion.clone();

                let p2 = provider_name.clone();
                let a2 = alias.clone();
                let k2 = key_name.clone();
                let m2 = model_id.clone();

                let sse_stream = stream
                    .map(move |chunk_result| {
                        chunk_result.map(|chunk| {
                            if let Some(usage) = &chunk.usage {
                                metrics::record_tokens(
                                    &p1,
                                    &a1,
                                    &k1,
                                    usage.prompt_tokens,
                                    usage.completion_tokens,
                                );
                                acc_p.store(
                                    usage.prompt_tokens,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                                acc_c.store(
                                    usage.completion_tokens,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                            }
                            let data = serde_json::to_string(&chunk).unwrap_or_default();
                            Event::default().data(data)
                        })
                    })
                    .chain(futures::stream::once(async move {
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

                        // Persist accumulated usage to database
                        let prompt = accumulated_prompt.load(std::sync::atomic::Ordering::Relaxed);
                        let completion =
                            accumulated_completion.load(std::sync::atomic::Ordering::Relaxed);
                        if prompt > 0 || completion > 0 {
                            usage_writer.record(UsageEvent {
                                client_id: stream_client_id,
                                request_id: stream_request_id,
                                model: stream_model,
                                provider: stream_provider,
                                prompt_tokens: prompt,
                                completion_tokens: completion,
                                latency_ms: Some((latency * 1000.0) as u64),
                            });

                            // Reconcile budget reservation with actual usage
                            if let (Some(ref cid), Some(ref period)) =
                                (&stream_client_id, &stream_budget_period)
                            {
                                budget_enforcer
                                    .reconcile_tokens(
                                        &cid.to_string(),
                                        period,
                                        stream_budget_reserved,
                                        prompt + completion,
                                    )
                                    .await;
                            }
                        }

                        tracing::info!(
                            model_alias = %a2,
                            provider = %p2,
                            model_id = %m2,
                            streaming = true,
                            status = 200,
                            latency_ms = (latency * 1000.0) as u64,
                            "request_completed"
                        );
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
        let result = dispatch_non_stream(&pool, &req, retry_config).await;
        let latency = start.elapsed().as_secs_f64();

        match result {
            Ok((resp, provider_name, model_id)) => {
                // Record tokens
                if let Some(usage) = &resp.usage {
                    metrics::record_tokens(
                        &provider_name,
                        &req.model,
                        &ctx.key_name,
                        usage.prompt_tokens,
                        usage.completion_tokens,
                    );

                    // Persist usage to database
                    state.usage_writer.record(UsageEvent {
                        client_id: ctx.client_id,
                        request_id: ctx.request_id.clone(),
                        model: req.model.clone(),
                        provider: provider_name.clone(),
                        prompt_tokens: usage.prompt_tokens,
                        completion_tokens: usage.completion_tokens,
                        latency_ms: Some((latency * 1000.0) as u64),
                    });

                    // Reconcile budget reservation with actual usage
                    if let (Some(ref cid), Some(ref period)) = (&ctx.client_id, &ctx.budget_period)
                    {
                        state
                            .budget_enforcer
                            .reconcile_tokens(
                                &cid.to_string(),
                                period,
                                ctx.budget_reserved_tokens,
                                usage.prompt_tokens + usage.completion_tokens,
                            )
                            .await;
                    }
                }
                REQUESTS_TOTAL
                    .with_label_values(&[
                        provider_name.as_str(),
                        req.model.as_str(),
                        model_id.as_str(),
                        "200",
                        ctx.key_name.as_str(),
                    ])
                    .inc();
                REQUEST_DURATION_SECONDS
                    .with_label_values(&[provider_name.as_str(), req.model.as_str(), "200"])
                    .observe(latency);

                tracing::info!(
                    request_id = %ctx.request_id,
                    key_name = %ctx.key_name,
                    model_alias = %req.model,
                    provider = %provider_name,
                    model_id = %model_id,
                    streaming = false,
                    status = 200,
                    latency_ms = (latency * 1000.0) as u64,
                    prompt_tokens = resp.usage.as_ref().map(|u| u.prompt_tokens).unwrap_or(0),
                    completion_tokens = resp.usage.as_ref().map(|u| u.completion_tokens).unwrap_or(0),
                    "request_completed"
                );

                Ok(Json(resp).into_response())
            }
            Err(e) => {
                record_error_metrics(&req.model, "", &e, start);
                Err(e)
            }
        }
    }
}

fn record_error_metrics(model_alias: &str, provider: &str, e: &ProxyError, start: Instant) {
    let latency = start.elapsed().as_secs_f64();
    let status_code = http_status_for_error(e).to_string();
    let error_type = error_type_label(e);

    REQUESTS_TOTAL
        .with_label_values(&[provider, model_alias, "", &status_code, ""])
        .inc();
    REQUEST_DURATION_SECONDS
        .with_label_values(&[provider, model_alias, &status_code])
        .observe(latency);
    ERRORS_TOTAL
        .with_label_values(&[provider, error_type])
        .inc();
}

fn http_status_for_error(e: &ProxyError) -> u16 {
    match e {
        ProxyError::Unauthorized(_) => 401,
        ProxyError::Forbidden(_) => 403,
        ProxyError::ModelNotFound(_) => 404,
        ProxyError::RateLimited(_) | ProxyError::BudgetExceeded(_) => 429,
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
        ProxyError::BudgetExceeded(_) => "budget_exceeded",
        ProxyError::CircuitOpen(_) => "circuit_open",
        ProxyError::ProviderError { .. } => "provider_error",
        ProxyError::UpstreamTimeout(_) => "upstream_timeout",
        ProxyError::StreamError(_) => "stream_error",
        ProxyError::HttpClientError(_) => "http_client_error",
        ProxyError::AwsError(_) => "aws_error",
        _ => "internal",
    }
}

// ── Non-streaming dispatch ────────────────────────────────────────────────────

/// Returns `(response, provider_name, model_id)` on success.
pub(crate) async fn dispatch_non_stream(
    pool: &RoutePool,
    req: &ChatCompletionRequest,
    retry_config: &RetryConfig,
) -> Result<(ChatCompletionResponse, String, String), ProxyError> {
    // Try primary targets
    if let Some(target) = pool.select_target() {
        let provider_name = target.provider.name().to_string();
        let model_id = target.model_id.clone();

        match try_non_stream(target, req, retry_config, &pool.alias).await {
            Ok(resp) => {
                target.circuit_breaker.record_success();
                return Ok((resp, provider_name, model_id));
            }
            Err(e) if is_retryable(&e) => {
                target.circuit_breaker.record_failure();
                tracing::warn!(
                    provider = %provider_name,
                    model_id = %model_id,
                    error = %e,
                    "Primary target failed — trying fallback chain"
                );
            }
            Err(e) => return Err(e),
        }
    }

    // Fallback chain
    for fallback in pool.fallbacks.iter().filter(|t| t.is_available()) {
        let provider_name = fallback.provider.name().to_string();
        let model_id = fallback.model_id.clone();

        match try_non_stream(fallback, req, retry_config, &pool.alias).await {
            Ok(resp) => {
                fallback.circuit_breaker.record_success();
                FALLBACK_TOTAL
                    .with_label_values(&[pool.alias.as_str(), "", provider_name.as_str()])
                    .inc();
                tracing::info!(
                    provider = %provider_name,
                    model_id = %model_id,
                    "Request served by fallback"
                );
                return Ok((resp, provider_name, model_id));
            }
            Err(e) => {
                fallback.circuit_breaker.record_failure();
                tracing::warn!(provider = %provider_name, error = %e, "Fallback failed");
            }
        }
    }

    Err(ProxyError::ProviderError {
        provider: pool.alias.clone(),
        status: StatusCode::BAD_GATEWAY.as_u16(),
        message: "All targets and fallbacks failed".to_string(),
    })
}

async fn try_non_stream(
    target: &RouteTarget,
    req: &ChatCompletionRequest,
    retry_config: &RetryConfig,
    model_alias: &str,
) -> Result<ChatCompletionResponse, ProxyError> {
    let provider = target.provider.clone();
    let model_id = target.model_id.clone();
    let provider_name = provider.name().to_string();

    execute_with_retry(retry_config, &provider_name, model_alias, move || {
        let provider = provider.clone();
        let model_id = model_id.clone();
        async move { provider.chat(req, &model_id).await }
    })
    .await
}

// ── Streaming dispatch ────────────────────────────────────────────────────────

/// Returns `(stream, provider_name, model_id)` on success.
pub(crate) async fn dispatch_stream(
    pool: &RoutePool,
    req: &ChatCompletionRequest,
    retry_config: &RetryConfig,
) -> Result<(ProviderStream, String, String), ProxyError> {
    // Try primary targets
    if let Some(target) = pool.select_target() {
        let provider_name = target.provider.name().to_string();
        let model_id = target.model_id.clone();

        match try_stream(target, req, retry_config, &pool.alias).await {
            Ok(stream) => {
                target.circuit_breaker.record_success();
                return Ok((stream, provider_name, model_id));
            }
            Err(e) if is_retryable(&e) => {
                target.circuit_breaker.record_failure();
                tracing::warn!(
                    provider = %provider_name,
                    error = %e,
                    "Primary streaming target failed — trying fallback"
                );
            }
            Err(e) => return Err(e),
        }
    }

    for fallback in pool.fallbacks.iter().filter(|t| t.is_available()) {
        let provider_name = fallback.provider.name().to_string();
        let model_id = fallback.model_id.clone();

        match try_stream(fallback, req, retry_config, &pool.alias).await {
            Ok(stream) => {
                fallback.circuit_breaker.record_success();
                FALLBACK_TOTAL
                    .with_label_values(&[pool.alias.as_str(), "", provider_name.as_str()])
                    .inc();
                tracing::info!(
                    provider = %provider_name,
                    "Streaming request served by fallback"
                );
                return Ok((stream, provider_name, model_id));
            }
            Err(e) => {
                fallback.circuit_breaker.record_failure();
                tracing::warn!(provider = %provider_name, error = %e, "Streaming fallback failed");
            }
        }
    }

    Err(ProxyError::ProviderError {
        provider: pool.alias.clone(),
        status: StatusCode::BAD_GATEWAY.as_u16(),
        message: "All targets and fallbacks failed".to_string(),
    })
}

async fn try_stream(
    target: &RouteTarget,
    req: &ChatCompletionRequest,
    retry_config: &RetryConfig,
    model_alias: &str,
) -> Result<ProviderStream, ProxyError> {
    let provider = target.provider.clone();
    let model_id = target.model_id.clone();
    let provider_name = provider.name().to_string();

    execute_with_retry(retry_config, &provider_name, model_alias, move || {
        let provider = provider.clone();
        let model_id = model_id.clone();
        async move { provider.chat_stream(req, &model_id).await }
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
