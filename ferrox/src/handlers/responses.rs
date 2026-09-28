//! `POST /v1/responses` — the OpenAI **Responses API** surface.
//!
//! A thin handler: the request is translated to the internal chat format
//! (`ferrox_providers::responses_types`), dispatched through the same
//! retry / failover / circuit-breaker pipeline as `/v1/chat/completions`, and
//! the chat answer is encoded back as a Responses object or event stream
//! (`ferrox_providers::responses_emitter`). Accounting goes through the shared
//! [`RequestFinalizer`]. All wire logic lives in `ferrox-providers`.
//!
//! The endpoint is stateless: nothing is stored, so `previous_response_id`,
//! `conversation`, `prompt`, `background`, hosted built-in tools and
//! Files-API inputs (`input_file`, `input_image` by `file_id`) are rejected
//! with an OpenAI-shaped 400 by the translation.

use std::time::Instant;

use axum::{
    body::Bytes,
    extract::{Extension, State},
    response::{sse::KeepAlive, IntoResponse, Response, Sse},
    Json,
};
use ferrox_providers::responses_emitter::{
    new_response_id, responses_stream_to_sse, to_responses_response, ResponsesEmitter,
};
use ferrox_providers::responses_types::{to_chat_completion_request, ResponsesRequest};
use futures::StreamExt as _;

use crate::error::ProxyError;
use crate::handlers::chat::{dispatch_non_stream, dispatch_stream, is_model_allowed};
use crate::handlers::finalize::{record_error_metrics, RequestFinalizer, Surface};
use crate::state::AppState;
use crate::types::RequestContext;

#[utoipa::path(
    post,
    path = "/v1/responses",
    tag = "OpenAI",
    security(("bearer_auth" = [])),
    request_body = ResponsesRequestCore,
    responses(
        (status = 200, description = "Response object. JSON body (mirrors the OpenAI Responses \
            API `response` object), or an SSE stream of typed `response.*` events ending in \
            exactly one of `response.completed` / `response.incomplete` / `response.failed` \
            when `stream=true` (no `[DONE]` sentinel)."),
        (status = 400, description = "Malformed body, or a stateful / unsupported feature \
            (`previous_response_id`, `conversation`, `prompt`, `background`, hosted built-in \
            tools, `input_file`, `input_image` by `file_id`)",
            body = ErrorResponse),
        (status = 401, description = "Missing or invalid credentials", body = ErrorResponse),
        (status = 403, description = "Model not permitted for this key", body = ErrorResponse),
        (status = 404, description = "Unknown model alias", body = ErrorResponse),
        (status = 429, description = "Rate limited or budget exceeded", body = ErrorResponse),
        (status = 502, description = "Upstream provider error", body = ErrorResponse),
    )
)]
pub async fn responses(
    State(state): State<AppState>,
    Extension(ctx): Extension<RequestContext>,
    body: Bytes,
) -> Result<Response, ProxyError> {
    let start = Instant::now();

    let req: ResponsesRequest = serde_json::from_slice(&body)?;

    if !is_model_allowed(&req.model, &ctx.allowed_models) {
        return Err(ProxyError::Forbidden(format!(
            "Key '{}' is not authorized to use model '{}'",
            ctx.key_name, req.model
        )));
    }

    let pool = state.router.resolve(&req.model)?;
    let internal_req = to_chat_completion_request(&req)?;
    let retry_config = &state.config.defaults.retry;
    let response_id = new_response_id();

    tracing::info!(
        request_id = %ctx.request_id,
        key_name = %ctx.key_name,
        model_alias = %req.model,
        response_id = %response_id,
        streaming = req.is_streaming(),
        "Dispatching Responses-format request"
    );

    if req.is_streaming() {
        match dispatch_stream(&pool, &internal_req, retry_config).await {
            Ok((stream, provider_name, model_id)) => {
                let finalizer = RequestFinalizer::new(
                    &state,
                    &ctx,
                    req.model.clone(),
                    provider_name,
                    model_id,
                    start,
                    Surface::Responses,
                );
                // The emitter drains the metered stream before it emits the
                // terminal event, so usage is recorded ahead of that frame.
                let emitter = ResponsesEmitter::new(&req, response_id);
                let sse_stream =
                    responses_stream_to_sse(emitter, finalizer.wrap_stream(stream).boxed());
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
        match dispatch_non_stream(&pool, &internal_req, retry_config).await {
            Ok((resp, provider_name, model_id)) => {
                RequestFinalizer::new(
                    &state,
                    &ctx,
                    req.model.clone(),
                    provider_name,
                    model_id,
                    start,
                    Surface::Responses,
                )
                .finish(resp.usage.as_ref())
                .await;
                Ok(Json(to_responses_response(resp, &req, response_id)).into_response())
            }
            Err(e) => {
                record_error_metrics(&req.model, "", &e, start);
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::{json, Value};
    use tokio::sync::mpsc;
    use tower::ServiceExt as _;

    use crate::config::Config;
    use crate::error::ProxyError;
    use crate::event_dispatcher::{EventDispatcher, TokenUsageEvent};
    use crate::providers::{ProviderAdapter, ProviderRegistry, ProviderStream};
    use crate::state::AppState;
    use crate::types::{ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse};
    use crate::usage_writer::{UsageEvent, UsageWriter};

    const KEY: &str = "sk-responses-test";

    /// Answers every request with "Hello" (11 prompt / 7 completion tokens)
    /// and remembers the last translated request it saw.
    struct OkProvider {
        seen: Mutex<Option<ChatCompletionRequest>>,
    }

    fn completion() -> ChatCompletionResponse {
        serde_json::from_value(json!({
            "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "m",
            "choices": [{"index": 0, "finish_reason": "stop",
                         "message": {"role": "assistant", "content": "Hello"}}],
            "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
        }))
        .unwrap()
    }

    fn chunk(delta: Value, finish: Option<&str>, usage: Option<Value>) -> ChatCompletionChunk {
        serde_json::from_value(json!({
            "id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            "usage": usage
        }))
        .unwrap()
    }

    #[async_trait]
    impl ProviderAdapter for OkProvider {
        fn name(&self) -> &str {
            "ok"
        }

        async fn chat(
            &self,
            req: &ChatCompletionRequest,
            _model_id: &str,
        ) -> Result<ChatCompletionResponse, ProxyError> {
            *self.seen.lock().unwrap() = Some(req.clone());
            Ok(completion())
        }

        async fn chat_stream(
            &self,
            req: &ChatCompletionRequest,
            _model_id: &str,
        ) -> Result<ProviderStream, ProxyError> {
            *self.seen.lock().unwrap() = Some(req.clone());
            let chunks = vec![
                Ok(chunk(
                    json!({"role": "assistant", "content": "Hel"}),
                    None,
                    None,
                )),
                Ok(chunk(json!({"content": "lo"}), Some("stop"), None)),
                Ok(chunk(
                    json!({}),
                    None,
                    Some(json!({"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18})),
                )),
            ];
            Ok(Box::pin(futures::stream::iter(chunks)))
        }
    }

    /// Always fails with a retryable upstream 503.
    struct DownProvider;

    fn down() -> ProxyError {
        ProxyError::ProviderError {
            provider: "down".to_string(),
            status: 503,
            message: "unavailable".to_string(),
        }
    }

    #[async_trait]
    impl ProviderAdapter for DownProvider {
        fn name(&self) -> &str {
            "down"
        }

        async fn chat(
            &self,
            _req: &ChatCompletionRequest,
            _model_id: &str,
        ) -> Result<ChatCompletionResponse, ProxyError> {
            Err(down())
        }

        async fn chat_stream(
            &self,
            _req: &ChatCompletionRequest,
            _model_id: &str,
        ) -> Result<ProviderStream, ProxyError> {
            Err(down())
        }
    }

    struct Harness {
        app: axum::Router,
        ok: Arc<OkProvider>,
        usage_rx: mpsc::Receiver<UsageEvent>,
        events_rx: mpsc::Receiver<TokenUsageEvent>,
    }

    /// A gateway with two providers (`ok`, `down`) and, per `alias`, a
    /// failover route whose primary is `down` and whose fallback is `ok` when
    /// the alias starts with `failover-`, else a single `ok` target.
    /// `rpm` sets a per-key rate limit. Unique aliases per test keep the global
    /// Prometheus statics from interfering.
    fn harness(alias: &str, allowed: &[&str], rpm: Option<u32>) -> Harness {
        let routing = if alias.starts_with("failover-") {
            json!({"strategy": "failover",
                   "targets": [{"provider": "down", "model_id": "down-v1"}],
                   "fallback": [{"provider": "ok", "model_id": "ok-v1"}]})
        } else {
            json!({"strategy": "round_robin",
                   "targets": [{"provider": "ok", "model_id": "ok-v1"}]})
        };
        let mut key = json!({"key": KEY, "name": "test-key", "allowed_models": allowed});
        if let Some(rpm) = rpm {
            key["rate_limit"] = json!({"requests_per_minute": rpm, "burst": rpm});
        }
        let config: Config = serde_json::from_value(json!({
            "defaults": {"retry": {"max_attempts": 1, "initial_backoff_ms": 1,
                                   "max_backoff_ms": 1, "jitter": false}},
            "providers": [{"name": "ok", "type": "openai"}, {"name": "down", "type": "openai"}],
            "models": [{"alias": alias, "routing": routing}],
            "virtual_keys": [key]
        }))
        .unwrap();

        let ok = Arc::new(OkProvider {
            seen: Mutex::new(None),
        });
        let mut registry: ProviderRegistry = HashMap::new();
        registry.insert("ok".to_string(), ok.clone() as Arc<dyn ProviderAdapter>);
        registry.insert("down".to_string(), Arc::new(DownProvider));
        let router = crate::router::ModelRouter::from_config(&config, &registry).unwrap();
        let (usage_writer, usage_rx) = UsageWriter::channel(16);
        let (event_dispatcher, events_rx) = EventDispatcher::channel(16);
        let state = AppState {
            config: Arc::new(config),
            providers: Arc::new(registry),
            router: Arc::new(router),
            rate_limit_backend: Arc::new(crate::ratelimit::MemoryBackend::new()),
            metrics: Arc::new(crate::metrics::Metrics::new()),
            ready: Arc::new(AtomicBool::new(true)),
            jwks_cache: Arc::new(crate::jwks::JwksCache::new(
                vec![],
                300,
                reqwest::Client::new(),
            )),
            usage_writer,
            budget_enforcer: Arc::new(crate::budget_enforcer::NoopBudgetEnforcer),
            event_dispatcher,
        };
        Harness {
            app: crate::server::build_router(state),
            ok,
            usage_rx,
            events_rx,
        }
    }

    async fn post(
        app: &axum::Router,
        key: Option<&str>,
        body: &str,
    ) -> (StatusCode, String, String) {
        let mut req = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("content-type", "application/json");
        if let Some(key) = key {
            req = req.header("authorization", format!("Bearer {key}"));
        }
        let resp = app
            .clone()
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            content_type,
            String::from_utf8(bytes.to_vec()).unwrap(),
        )
    }

    /// `(event name, data)` pairs of an SSE body, skipping comments.
    fn sse_events(body: &str) -> Vec<(String, Value)> {
        body.split("\n\n")
            .filter_map(|frame| {
                let mut event = None;
                let mut data = None;
                for line in frame.lines() {
                    if let Some(e) = line.strip_prefix("event: ") {
                        event = Some(e.to_string());
                    } else if let Some(d) = line.strip_prefix("data: ") {
                        data = Some(serde_json::from_str(d).unwrap());
                    }
                }
                Some((event?, data?))
            })
            .collect()
    }

    fn error_body(body: &str) -> Value {
        serde_json::from_str::<Value>(body).unwrap()["error"].clone()
    }

    #[tokio::test]
    async fn non_streaming_returns_a_response_object_and_accounts_usage() {
        let alias = "resp-nonstream";
        let mut h = harness(alias, &["*"], None);
        let body = json!({"model": alias, "input": "Hi", "instructions": "Be brief."});

        let (status, content_type, body) = post(&h.app, Some(KEY), &body.to_string()).await;

        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(content_type.starts_with("application/json"));
        let resp: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(resp["object"], "response");
        assert!(resp["id"].as_str().unwrap().starts_with("resp_"));
        assert_eq!(resp["status"], "completed");
        assert_eq!(resp["model"], alias);
        assert_eq!(resp["output"][0]["type"], "message");
        assert_eq!(resp["output"][0]["content"][0]["type"], "output_text");
        assert_eq!(resp["output"][0]["content"][0]["text"], "Hello");
        assert_eq!(resp["usage"]["input_tokens"], 11);
        assert_eq!(resp["usage"]["output_tokens"], 7);

        // The provider saw the translated chat request.
        let seen = h.ok.seen.lock().unwrap().clone().unwrap();
        assert_eq!(seen.messages[0].role, "system");
        assert_eq!(seen.messages[1].role, "user");

        // Same accounting as the other surfaces: usage_log row + webhook.
        let usage = h.usage_rx.try_recv().expect("usage_log event");
        assert_eq!(
            (usage.model.as_str(), usage.provider.as_str()),
            (alias, "ok")
        );
        assert_eq!((usage.prompt_tokens, usage.completion_tokens), (11, 7));
        let event = h.events_rx.try_recv().expect("token_usage webhook");
        assert_eq!(event.total_tokens, 18);
    }

    #[tokio::test]
    async fn streaming_emits_typed_events_without_done_and_accounts_usage() {
        let alias = "resp-stream";
        let mut h = harness(alias, &["*"], None);
        let body = json!({"model": alias, "input": "Hi", "stream": true});

        let (status, content_type, body) = post(&h.app, Some(KEY), &body.to_string()).await;

        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            content_type.starts_with("text/event-stream"),
            "{content_type}"
        );
        assert!(
            !body.contains("[DONE]"),
            "Responses streams carry no [DONE]"
        );
        let events = sse_events(&body);
        let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names.first(), Some(&"response.created"));
        assert_eq!(names.last(), Some(&"response.completed"));
        for (name, data) in &events {
            assert_eq!(&data["type"], name, "SSE event name must equal its type");
        }
        let text: String = events
            .iter()
            .filter(|(n, _)| n == "response.output_text.delta")
            .map(|(_, d)| d["delta"].as_str().unwrap())
            .collect();
        assert_eq!(text, "Hello");
        let completed = &events.last().unwrap().1["response"];
        assert_eq!(completed["usage"]["input_tokens"], 11);
        assert_eq!(completed["usage"]["output_tokens"], 7);

        let usage = h.usage_rx.try_recv().expect("usage_log event");
        assert_eq!((usage.prompt_tokens, usage.completion_tokens), (11, 7));
        assert!(h.events_rx.try_recv().is_ok(), "token_usage webhook");
    }

    #[tokio::test]
    async fn validation_failures_are_openai_shaped_400s() {
        let alias = "resp-invalid";
        let h = harness(alias, &["*"], None);

        let (status, _, body) = post(&h.app, Some(KEY), "{not json").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(error_body(&body)["message"].is_string());

        let cases = [
            (
                json!({"model": alias, "input": "Hi", "previous_response_id": "resp_1"}),
                "previous_response_id",
            ),
            (
                json!({"model": alias, "input": "Hi", "tools": [{"type": "web_search"}]}),
                "tools[0].type",
            ),
            (json!({"model": alias}), "input"),
        ];
        for (req, param) in cases {
            let (status, _, body) = post(&h.app, Some(KEY), &req.to_string()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{req}: {body}");
            let err = error_body(&body);
            assert_eq!(err["type"], "invalid_request_error", "{body}");
            assert_eq!(err["param"], param, "{body}");
        }
        assert!(
            h.ok.seen.lock().unwrap().is_none(),
            "rejected requests must never reach a provider"
        );
    }

    #[tokio::test]
    async fn auth_and_model_access_apply() {
        let alias = "resp-auth";
        let h = harness(alias, &["some-other-model"], None);
        let body = json!({"model": alias, "input": "Hi"}).to_string();

        let (status, _, _) = post(&h.app, None, &body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _, _) = post(&h.app, Some("sk-wrong"), &body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, _, body) = post(&h.app, Some(KEY), &body).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(error_body(&body)["type"], "forbidden");

        let unknown = json!({"model": "no-such-alias", "input": "Hi"}).to_string();
        let h = harness(alias, &["*"], None);
        let (status, _, _) = post(&h.app, Some(KEY), &unknown).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn virtual_key_rate_limit_applies() {
        let alias = "resp-ratelimit";
        let h = harness(alias, &["*"], Some(1));
        let body = json!({"model": alias, "input": "Hi"}).to_string();

        let (first, _, _) = post(&h.app, Some(KEY), &body).await;
        let (second, _, _) = post(&h.app, Some(KEY), &body).await;
        assert_eq!(first, StatusCode::OK);
        assert_eq!(second, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn failover_serves_the_fallback_in_responses_format() {
        let alias = "failover-resp";
        let mut h = harness(alias, &["*"], None);

        let body = json!({"model": alias, "input": "Hi"}).to_string();
        let (status, _, body) = post(&h.app, Some(KEY), &body).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let resp: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(resp["object"], "response");
        assert_eq!(resp["output"][0]["content"][0]["text"], "Hello");
        assert_eq!(h.usage_rx.try_recv().unwrap().provider, "ok");

        let body = json!({"model": alias, "input": "Hi", "stream": true}).to_string();
        let (status, _, body) = post(&h.app, Some(KEY), &body).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let events = sse_events(&body);
        assert_eq!(events.last().unwrap().0, "response.completed");
        assert_eq!(h.usage_rx.try_recv().unwrap().provider, "ok");
    }
}
