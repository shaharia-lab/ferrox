//! `POST /v1/responses` — the OpenAI **Responses API** surface.
//!
//! A thin handler. Per attempt, the target decides how the request reaches
//! it:
//!
//! - **translate** (every provider, the default): the request is translated
//!   to the internal chat format (`ferrox_providers::responses_types`), and
//!   the chat answer is encoded back as a Responses object or event stream
//!   (`ferrox_providers::responses_emitter`);
//! - **native** (`type: openai` providers configured with `responses:
//!   native`): the client's body goes to the provider's own `/responses`
//!   with only `model` replaced, and its answer is passed through verbatim.
//!
//! Both run through the same retry / failover / circuit-breaker pipeline as
//! `/v1/chat/completions`, so a failover from a native target to a
//! translate-only one still returns a valid Responses answer. Accounting goes
//! through the shared [`RequestFinalizer`]. All wire logic lives in
//! `ferrox-providers`.
//!
//! The endpoint is stateless: Ferrox stores nothing, so `previous_response_id`,
//! `conversation`, `prompt` and `background` are rejected with an
//! OpenAI-shaped 400, and so is `store: true` on an alias with a native
//! target (where an omitted `store` follows the upstream's default, so
//! clients should send `store: false`). Hosted built-in tools and Files-API
//! inputs (`input_file`, `input_image` by `file_id`) cannot be translated:
//! they are served only by native targets, and rejected with a 400 when none
//! can take the request.

use std::time::Instant;

use axum::{
    body::Bytes,
    extract::{Extension, State},
    http::{header::CONTENT_TYPE, HeaderMap},
    response::{
        sse::{Event, KeepAlive},
        IntoResponse, Response, Sse,
    },
    Json,
};
use ferrox_providers::responses_emitter::{
    native_stream_with_terminal_error, new_response_id, responses_stream_to_sse,
    to_responses_response, ResponsesEmitter,
};
use ferrox_providers::responses_types::{
    reject_native_stateful_features, to_chat_completion_request, ResponsesRequest,
};
use futures::StreamExt as _;
use serde_json::{Map, Value};

use crate::budget_enforcer::BudgetReservation;
use crate::classifier::{skip_requested, ClassifierInput};
use crate::error::ProxyError;
use crate::handlers::chat::{dispatch, is_model_allowed};
use crate::handlers::finalize::{record_error_metrics, RequestFinalizer, Surface};
use crate::lb::{RoutePool, RouteTarget};
use crate::state::AppState;
use crate::types::RequestContext;

#[utoipa::path(
    post,
    path = "/v1/responses",
    tag = "OpenAI",
    security(("bearer_auth" = [])),
    request_body = ResponsesRequestCore,
    params(
        ("x-ferrox-classifier" = Option<String>, Header, nullable = false, description = "Set to `skip` \
            (case-insensitive) to skip classification for this request: a classified alias \
            is then served by its `fallback_alias` and the classifier is not called. Any \
            other value, and the header on a statically routed alias, has no effect."),
    ),
    responses(
        (status = 200, description = "Response object. JSON body (mirrors the OpenAI Responses \
            API `response` object), or an SSE stream of typed `response.*` events ending in \
            exactly one of `response.completed` / `response.incomplete` / `response.failed` \
            when `stream=true` (no `[DONE]` sentinel). From a `responses: native` provider, \
            the upstream's own body or events, passed through."),
        (status = 400, description = "Malformed body, or a stateful / unsupported feature \
            (`previous_response_id`, `conversation`, `prompt`, `background`; `store: true` \
            on an alias with a native target; hosted built-in tools, `input_file` and \
            `input_image` by `file_id` when no native target can serve the request)",
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
    reservation: Option<Extension<BudgetReservation>>,
    headers: HeaderMap,
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

    let decision = state
        .resolver
        .resolve(&req.model, skip_requested(&headers), |max_input_chars| {
            ClassifierInput::from_responses(&req, max_input_chars)
        })
        .await?;
    decision.log_classification(&ctx.request_id);
    let pool = &decision.pool;
    let model_alias = decision.served_alias();
    let plan = Plan::new(pool, &req, &body)?;
    let retry_config = &state.config.defaults.retry;

    tracing::info!(
        request_id = %ctx.request_id,
        key_name = %ctx.key_name,
        model_alias = %model_alias,
        streaming = req.is_streaming(),
        native_capable = plan.native.is_some(),
        "Dispatching Responses-format request"
    );

    // Only a translated answer carries a gateway id; a native one keeps the
    // upstream's. Logged when assigned, so it correlates with `request_id`.
    let assign_response_id = || {
        let response_id = new_response_id();
        tracing::info!(
            request_id = %ctx.request_id,
            response_id = %response_id,
            "Responses-format response id assigned"
        );
        response_id
    };

    // Only one of the paths below builds a finalizer; the clone is a
    // refcount bump, and the reservation's claim settles it once.
    let reservation = reservation.map(|Extension(r)| r);
    let finalizer = |provider_name, model_id| {
        RequestFinalizer::new(
            &state,
            &ctx,
            reservation.clone(),
            model_alias.to_string(),
            provider_name,
            model_id,
            start,
            Surface::Responses,
        )
    };

    if req.is_streaming() {
        let served = dispatch(
            pool,
            retry_config,
            true,
            |t| plan.skip(t),
            |provider, model_id| {
                let native = plan.native_for(provider.as_ref());
                let translated = plan.translated();
                async move {
                    match (native, translated) {
                        (Some(body), _) => provider
                            .responses_stream(body, &model_id)
                            .await
                            .map(Served::Native),
                        (None, Some(req)) => provider
                            .chat_stream(req, &model_id)
                            .await
                            .map(Served::Translated),
                        (None, None) => Err(Plan::unreachable()),
                    }
                }
            },
        )
        .await;
        match served {
            Ok((Served::Native(events), provider_name, model_id)) => {
                // Verbatim pass-through. The finalizer reads usage from the
                // terminal event and holds that event back until the request
                // is settled; an upstream failure part-way ends the stream
                // with an `error` event.
                let sse_stream = native_stream_with_terminal_error(
                    finalizer(provider_name, model_id).wrap_stream(events),
                )
                .map(|e| {
                    let frame = Event::default().data(e.data);
                    Ok::<_, ProxyError>(match e.event {
                        Some(name) => frame.event(name),
                        None => frame,
                    })
                });
                Ok(Sse::new(sse_stream)
                    .keep_alive(KeepAlive::default())
                    .into_response())
            }
            Ok((Served::Translated(stream), provider_name, model_id)) => {
                // The emitter drains the metered stream before it emits the
                // terminal event, so usage is recorded ahead of that frame.
                let emitter = ResponsesEmitter::new(&req, assign_response_id());
                let sse_stream = responses_stream_to_sse(
                    emitter,
                    finalizer(provider_name, model_id)
                        .wrap_stream(stream)
                        .boxed(),
                );
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
        let served = dispatch(
            pool,
            retry_config,
            false,
            |t| plan.skip(t),
            |provider, model_id| {
                let native = plan.native_for(provider.as_ref());
                let translated = plan.translated();
                async move {
                    match (native, translated) {
                        (Some(body), _) => provider
                            .responses(body, &model_id)
                            .await
                            .map(Served::Native),
                        (None, Some(req)) => {
                            provider.chat(req, &model_id).await.map(Served::Translated)
                        }
                        (None, None) => Err(Plan::unreachable()),
                    }
                }
            },
        )
        .await;
        match served {
            Ok((Served::Native(resp), provider_name, model_id)) => {
                finalizer(provider_name, model_id)
                    .finish(resp.usage.as_ref())
                    .await;
                Ok(([(CONTENT_TYPE, "application/json")], resp.body).into_response())
            }
            Ok((Served::Translated(resp), provider_name, model_id)) => {
                finalizer(provider_name, model_id)
                    .finish(resp.usage.as_ref())
                    .await;
                Ok(Json(to_responses_response(resp, &req, assign_response_id())).into_response())
            }
            Err(e) => {
                record_error_metrics(model_alias, "", &e, start);
                Err(e)
            }
        }
    }
}

/// What one attempt produced: the upstream's own Responses answer, or a chat
/// answer still to be encoded as one.
enum Served<N, C> {
    Native(N),
    Translated(C),
}

/// How this request can reach each target of its pool, decided once.
struct Plan {
    /// The client's body, when the pool has a native target to send it to.
    native: Option<Map<String, Value>>,
    /// The chat translation, or — when the pool has a native target — why
    /// the request cannot be translated (`message`, `param`), so that only
    /// the native targets are tried. `None` when every target is native: the
    /// translation would never be used, so it is not paid for.
    translated: Option<Result<crate::types::ChatCompletionRequest, (String, Option<String>)>>,
}

impl Plan {
    fn new(pool: &RoutePool, req: &ResponsesRequest, body: &Bytes) -> Result<Self, ProxyError> {
        let mut targets = pool.targets.iter().chain(&pool.fallbacks);
        let native_capable = targets
            .clone()
            .any(|t| t.provider.supports_native_responses());
        if !native_capable {
            // Translate-only pool: exactly the pre-native behaviour.
            return Ok(Self {
                native: None,
                translated: Some(Ok(to_chat_completion_request(req)?)),
            });
        }

        reject_native_stateful_features(req)?;
        let native = serde_json::from_slice::<Map<String, Value>>(body)?;
        let translated = if targets.any(|t| !t.provider.supports_native_responses()) {
            Some(match to_chat_completion_request(req) {
                Ok(r) => Ok(r),
                Err(ProxyError::InvalidRequest { message, param }) => Err((message, param)),
                Err(e) => return Err(e),
            })
        } else {
            None
        };
        Ok(Self {
            native: Some(native),
            translated,
        })
    }

    /// The chat translation, when there is one to send.
    fn translated(&self) -> Option<&crate::types::ChatCompletionRequest> {
        self.translated.as_ref().and_then(|t| t.as_ref().ok())
    }

    /// The body to send natively, when `provider` takes it.
    fn native_for(
        &self,
        provider: &dyn crate::providers::ProviderAdapter,
    ) -> Option<&Map<String, Value>> {
        self.native
            .as_ref()
            .filter(|_| provider.supports_native_responses())
    }

    /// Why `target` cannot serve this request: it is translate-only and the
    /// request does not translate.
    fn skip(&self, target: &RouteTarget) -> Option<ProxyError> {
        match &self.translated {
            Some(Err((message, param))) if !target.provider.supports_native_responses() => {
                Some(ProxyError::InvalidRequest {
                    message: message.clone(),
                    param: param.clone(),
                })
            }
            _ => None,
        }
    }

    /// An attempt with neither a native body nor a translation — prevented by
    /// [`skip`](Self::skip).
    fn unreachable() -> ProxyError {
        ProxyError::ConfigError("no way to serve this Responses request".to_string())
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

    use super::Plan;
    use crate::config::Config;
    use crate::error::ProxyError;
    use crate::event_dispatcher::{EventDispatcher, TokenUsageEvent};
    use crate::providers::{ProviderAdapter, ProviderRegistry, ProviderStream};
    use crate::state::AppState;
    use crate::types::{ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse};
    use crate::usage_writer::{UsageEvent, UsageWriter};
    use axum::body::Bytes;
    use ferrox_providers::responses_types::ResponsesRequest;

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
        build_harness(config, registry, ok)
    }

    fn build_harness(config: Config, registry: ProviderRegistry, ok: Arc<OkProvider>) -> Harness {
        let router = crate::router::ModelRouter::from_config(&config, &registry).unwrap();
        let resolver = crate::classifier::RouteResolver::build(&config, router).unwrap();
        let (usage_writer, usage_rx) = UsageWriter::channel(16);
        let (event_dispatcher, events_rx) = EventDispatcher::channel(16);
        let state = AppState {
            config: Arc::new(config),
            providers: Arc::new(registry),
            resolver: Arc::new(resolver),
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

    // ── native passthrough (`responses: native`) ────────────────────────────

    /// A mock OpenAI-protocol upstream serving `POST /v1/responses`: it
    /// records each request and answers with `status` — a JSON `response` or
    /// an SSE stream when the request asks for one, or an error body.
    struct MockUpstream {
        base_url: String,
        seen: Seen,
    }

    /// Each request the mock upstream received: `(path, body)`.
    type Seen = Arc<Mutex<Vec<(String, Value)>>>;

    const UPSTREAM_USAGE: &str = r#"{"input_tokens":13,"input_tokens_details":{"cached_tokens":4},"output_tokens":5,"output_tokens_details":{"reasoning_tokens":2},"total_tokens":18}"#;

    fn upstream_json() -> String {
        format!(
            r#"{{"id":"resp_up","object":"response","status":"completed","model":"up-v1","output":[{{"type":"web_search_call","id":"ws_1","status":"completed"}}],"usage":{UPSTREAM_USAGE}}}"#
        )
    }

    fn upstream_sse() -> String {
        [
            r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_up","status":"in_progress"}}"#
                .to_string(),
            r#"event: response.output_text.delta
data: {"type":"response.output_text.delta","delta":"Hi"}"#
                .to_string(),
            format!(
                "event: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_up\",\"status\":\"completed\",\"usage\":{UPSTREAM_USAGE}}}}}"
            ),
        ]
        .join("\n\n")
            + "\n\n"
    }

    async fn mock_upstream(status: u16) -> MockUpstream {
        use axum::extract::State as S;
        use axum::http::Uri;

        let seen: Seen = Arc::default();
        let app = axum::Router::new()
            .route(
                "/v1/responses",
                axum::routing::post(
                    move |S(seen): S<Seen>, uri: Uri, body: axum::body::Bytes| async move {
                        let body: Value = serde_json::from_slice(&body).unwrap();
                        let stream = body["stream"] == true;
                        seen.lock().unwrap().push((uri.path().to_string(), body));
                        let status = StatusCode::from_u16(status).unwrap();
                        if !status.is_success() {
                            return (
                                status,
                                [("content-type", "application/json")],
                                "{}".to_string(),
                            );
                        }
                        if stream {
                            (
                                status,
                                [("content-type", "text/event-stream")],
                                upstream_sse(),
                            )
                        } else {
                            (
                                status,
                                [("content-type", "application/json")],
                                upstream_json(),
                            )
                        }
                    },
                ),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        MockUpstream {
            base_url: format!("http://{addr}/v1"),
            seen,
        }
    }

    /// A gateway whose `native` provider is a real `type: openai` adapter with
    /// `responses: native` pointed at `upstream`, next to the `ok` / `down`
    /// translate-only mocks. `routing` is the alias's routing block.
    async fn native_harness(alias: &str, routing: Value, upstream: &MockUpstream) -> Harness {
        let config: Config = serde_json::from_value(json!({
            "defaults": {"retry": {"max_attempts": 1, "initial_backoff_ms": 1,
                                   "max_backoff_ms": 1, "jitter": false}},
            "providers": [
                {"name": "native", "type": "openai", "api_key": "sk-up",
                 "base_url": upstream.base_url, "responses": "native"},
                {"name": "ok", "type": "openai"}
            ],
            "models": [{"alias": alias, "routing": routing}],
            "virtual_keys": [{"key": KEY, "name": "test-key", "allowed_models": ["*"]}]
        }))
        .unwrap();
        crate::config::validate(&config).unwrap();

        let mut registry =
            crate::providers::build_registry(&config.providers[..1], &config.defaults)
                .await
                .unwrap();
        let ok = Arc::new(OkProvider {
            seen: Mutex::new(None),
        });
        registry.insert("ok".to_string(), ok.clone() as Arc<dyn ProviderAdapter>);
        registry.insert("down".to_string(), Arc::new(DownProvider));
        build_harness(config, registry, ok)
    }

    fn native_only() -> Value {
        json!({"strategy": "round_robin",
               "targets": [{"provider": "native", "model_id": "up-v1"}]})
    }

    /// Everything translation cannot carry: a hosted tool, `include`, and a
    /// field this gateway has never heard of.
    fn native_body(alias: &str, stream: bool) -> Value {
        json!({
            "model": alias,
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "Hi"}]}],
            "tools": [{"type": "web_search"}],
            "include": ["reasoning.encrypted_content"],
            "reasoning": {"effort": "high"},
            "store": false,
            "stream": stream,
            "x_brand_new_field": {"nested": [1, 2]}
        })
    }

    #[tokio::test]
    async fn native_non_stream_forwards_the_body_and_passes_the_response_through() {
        let alias = "resp-native-nonstream";
        let upstream = mock_upstream(200).await;
        let mut h = native_harness(alias, native_only(), &upstream).await;
        let body = native_body(alias, false);

        let (status, content_type, resp) = post(&h.app, Some(KEY), &body.to_string()).await;

        assert_eq!(status, StatusCode::OK, "{resp}");
        assert!(content_type.starts_with("application/json"));
        assert_eq!(
            resp,
            upstream_json(),
            "the upstream body is passed through verbatim"
        );

        // The upstream got the client's body with only `model` replaced.
        let seen = upstream.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "/v1/responses");
        let mut expected = body;
        expected["model"] = json!("up-v1");
        assert_eq!(seen[0].1, expected);
        assert!(h.ok.seen.lock().unwrap().is_none());

        let usage = h.usage_rx.try_recv().expect("usage_log event");
        assert_eq!(
            (usage.model.as_str(), usage.provider.as_str()),
            (alias, "native")
        );
        assert_eq!((usage.prompt_tokens, usage.completion_tokens), (13, 5));
        assert_eq!(usage.cache_read_tokens, 4);
        let event = h.events_rx.try_recv().expect("token_usage webhook");
        assert_eq!(event.total_tokens, 18);
        assert_eq!(event.cache_read_tokens, Some(4));
    }

    #[tokio::test]
    async fn native_stream_passes_events_through_and_accounts_the_terminal_usage() {
        let alias = "resp-native-stream";
        let upstream = mock_upstream(200).await;
        let mut h = native_harness(alias, native_only(), &upstream).await;

        let (status, content_type, body) =
            post(&h.app, Some(KEY), &native_body(alias, true).to_string()).await;

        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(content_type.starts_with("text/event-stream"));
        assert!(!body.contains("[DONE]"));
        assert_eq!(
            sse_events(&body),
            sse_events(&upstream_sse()),
            "events are forwarded verbatim"
        );
        assert_eq!(upstream.seen.lock().unwrap()[0].1["stream"], true);

        let usage = h.usage_rx.try_recv().expect("usage_log event");
        assert_eq!((usage.prompt_tokens, usage.completion_tokens), (13, 5));
        assert_eq!(usage.provider, "native");
        assert!(h.events_rx.try_recv().is_ok(), "token_usage webhook");
    }

    #[tokio::test]
    async fn failover_from_a_native_target_to_a_translate_only_one_returns_responses() {
        let alias = "resp-native-failover";
        let upstream = mock_upstream(503).await;
        let routing = json!({"strategy": "failover",
                             "targets": [{"provider": "native", "model_id": "up-v1"}],
                             "fallback": [{"provider": "ok", "model_id": "ok-v1"}]});
        let mut h = native_harness(alias, routing, &upstream).await;

        let body = json!({"model": alias, "input": "Hi"}).to_string();
        let (status, _, resp) = post(&h.app, Some(KEY), &body).await;
        assert_eq!(status, StatusCode::OK, "{resp}");
        let resp: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(resp["object"], "response");
        assert_eq!(resp["output"][0]["content"][0]["text"], "Hello");
        assert_eq!(h.usage_rx.try_recv().unwrap().provider, "ok");

        let body = json!({"model": alias, "input": "Hi", "stream": true}).to_string();
        let (status, _, resp) = post(&h.app, Some(KEY), &body).await;
        assert_eq!(status, StatusCode::OK, "{resp}");
        let events = sse_events(&resp);
        assert_eq!(events.first().unwrap().0, "response.created");
        assert_eq!(events.last().unwrap().0, "response.completed");
        assert_eq!(h.usage_rx.try_recv().unwrap().provider, "ok");

        // Both attempts reached the native upstream first.
        assert_eq!(upstream.seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn untranslatable_requests_skip_translate_only_targets() {
        // Primary translate-only, fallback native: a hosted tool goes
        // straight to the native fallback.
        let alias = "resp-native-skip";
        let upstream = mock_upstream(200).await;
        let routing = json!({"strategy": "failover",
                             "targets": [{"provider": "ok", "model_id": "ok-v1"}],
                             "fallback": [{"provider": "native", "model_id": "up-v1"}]});
        let h = native_harness(alias, routing, &upstream).await;
        let (status, _, resp) =
            post(&h.app, Some(KEY), &native_body(alias, false).to_string()).await;
        assert_eq!(status, StatusCode::OK, "{resp}");
        assert_eq!(upstream.seen.lock().unwrap().len(), 1);
        assert!(h.ok.seen.lock().unwrap().is_none());

        // Native primary down, translate-only fallback: nothing can serve it.
        let alias = "resp-native-skip-down";
        let upstream = mock_upstream(503).await;
        let routing = json!({"strategy": "failover",
                             "targets": [{"provider": "native", "model_id": "up-v1"}],
                             "fallback": [{"provider": "ok", "model_id": "ok-v1"}]});
        let h = native_harness(alias, routing, &upstream).await;
        let (status, _, _) = post(&h.app, Some(KEY), &native_body(alias, false).to_string()).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(h.ok.seen.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn untranslatable_requests_select_a_native_primary() {
        // The strategy picks among the targets that can take the request, so
        // a hosted tool never lands on the translate-only primary.
        for (alias, strategy) in [
            ("resp-native-rr", "round_robin"),
            ("resp-native-fo", "failover"),
        ] {
            let upstream = mock_upstream(200).await;
            let routing = json!({"strategy": strategy,
                                 "targets": [{"provider": "ok", "model_id": "ok-v1"},
                                             {"provider": "native", "model_id": "up-v1"}]});
            let h = native_harness(alias, routing, &upstream).await;
            for _ in 0..4 {
                let (status, _, resp) =
                    post(&h.app, Some(KEY), &native_body(alias, false).to_string()).await;
                assert_eq!(status, StatusCode::OK, "{strategy}: {resp}");
            }
            assert_eq!(upstream.seen.lock().unwrap().len(), 4, "{strategy}");
            assert!(h.ok.seen.lock().unwrap().is_none(), "{strategy}");
        }
    }

    /// A mock upstream that starts a Responses stream and then drops the
    /// connection part-way: raw HTTP/1.1, one chunk of a chunked body, and a
    /// close without the terminating chunk.
    async fn dropping_upstream() -> MockUpstream {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let event = "event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0}\n\n";
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                        transfer-encoding: chunked\r\n\r\n";
            let chunk = format!("{head}{:x}\r\n{event}\r\n", event.len());
            socket.write_all(chunk.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
        });
        MockUpstream {
            base_url: format!("http://{addr}/v1"),
            seen: Arc::default(),
        }
    }

    #[tokio::test]
    async fn a_native_stream_cut_part_way_ends_with_an_error_event() {
        let alias = "resp-native-cut";
        let upstream = dropping_upstream().await;
        let h = native_harness(alias, native_only(), &upstream).await;

        let (status, _, body) =
            post(&h.app, Some(KEY), &native_body(alias, true).to_string()).await;

        assert_eq!(status, StatusCode::OK, "{body}");
        let events = sse_events(&body);
        let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["response.created", "error"], "{body}");
        assert_eq!(events[1].1["type"], "error");
        assert_eq!(events[1].1["sequence_number"], 1);
    }

    #[tokio::test]
    async fn skipping_a_half_open_target_leaves_its_probe_slot_free() {
        let config: Config = serde_json::from_value(json!({
            "defaults": {"retry": {"max_attempts": 1, "initial_backoff_ms": 1,
                                   "max_backoff_ms": 1, "jitter": false}},
            "providers": [
                {"name": "down", "type": "openai"},
                {"name": "ok", "type": "openai",
                 "circuit_breaker": {"failure_threshold": 1, "recovery_timeout_secs": 0}}
            ],
            "models": [{"alias": "resp-skip-probe", "routing": {
                "strategy": "failover",
                "targets": [{"provider": "down", "model_id": "down-v1"}],
                "fallback": [{"provider": "ok", "model_id": "ok-v1"}]}}]
        }))
        .unwrap();
        let mut registry: ProviderRegistry = HashMap::new();
        registry.insert("down".to_string(), Arc::new(DownProvider));
        registry.insert(
            "ok".to_string(),
            Arc::new(OkProvider {
                seen: Mutex::new(None),
            }),
        );
        let router = crate::router::ModelRouter::from_config(&config, &registry).unwrap();
        let pool = router.resolve("resp-skip-probe").unwrap();
        // Open the fallback's breaker; with no recovery timeout, the next
        // request admitted on it moves it to half-open and claims the probe.
        pool.fallbacks[0].circuit_breaker.record_failure();

        let req: ChatCompletionRequest =
            serde_json::from_value(json!({"model": "m", "messages": []})).unwrap();
        let result = crate::handlers::chat::dispatch(
            &pool,
            &config.defaults.retry,
            false,
            |t| {
                (t.provider.name() == "ok").then(|| ProxyError::InvalidRequest {
                    message: "untranslatable".to_string(),
                    param: None,
                })
            },
            |provider, model_id| {
                let req = &req;
                async move { provider.chat(req, &model_id).await }
            },
        )
        .await;
        assert!(result.is_err());

        assert!(
            pool.fallbacks[0].can_serve(),
            "the skipped target's probe slot must still be free"
        );
    }

    #[tokio::test]
    async fn an_unavailable_capable_target_is_a_502_not_the_skip_reason() {
        // The only target able to serve the request has its breaker open;
        // the other is skipped. That is a retryable outage, not a 400.
        let config: Config = serde_json::from_value(json!({
            "providers": [
                {"name": "down", "type": "openai",
                 "circuit_breaker": {"failure_threshold": 1, "recovery_timeout_secs": 3600}},
                {"name": "ok", "type": "openai"}
            ],
            "models": [{"alias": "resp-skip-open", "routing": {
                "strategy": "failover",
                "targets": [{"provider": "down", "model_id": "down-v1"}],
                "fallback": [{"provider": "ok", "model_id": "ok-v1"}]}}]
        }))
        .unwrap();
        let mut registry: ProviderRegistry = HashMap::new();
        registry.insert("down".to_string(), Arc::new(DownProvider));
        registry.insert(
            "ok".to_string(),
            Arc::new(OkProvider {
                seen: Mutex::new(None),
            }),
        );
        let router = crate::router::ModelRouter::from_config(&config, &registry).unwrap();
        let pool = router.resolve("resp-skip-open").unwrap();
        pool.targets[0].circuit_breaker.record_failure();

        let req: ChatCompletionRequest =
            serde_json::from_value(json!({"model": "m", "messages": []})).unwrap();
        let skip_ok = |t: &crate::lb::RouteTarget| {
            (t.provider.name() == "ok").then(|| ProxyError::InvalidRequest {
                message: "untranslatable".to_string(),
                param: None,
            })
        };
        let result = crate::handlers::chat::dispatch(
            &pool,
            &config.defaults.retry,
            false,
            skip_ok,
            |provider, model_id| {
                let req = &req;
                async move { provider.chat(req, &model_id).await }
            },
        )
        .await;
        assert!(
            matches!(result, Err(ProxyError::ProviderError { status: 502, .. })),
            "{:?}",
            result.err()
        );

        // Every target skipped: the skip reason is the answer.
        let result = crate::handlers::chat::dispatch(
            &pool,
            &config.defaults.retry,
            false,
            |_| {
                Some(ProxyError::InvalidRequest {
                    message: "untranslatable".to_string(),
                    param: None,
                })
            },
            |provider, model_id| {
                let req = &req;
                async move { provider.chat(req, &model_id).await }
            },
        )
        .await;
        assert!(matches!(result, Err(ProxyError::InvalidRequest { .. })));
    }

    #[tokio::test]
    async fn an_all_native_pool_skips_the_translation() {
        let config: Config = serde_json::from_value(json!({
            "providers": [
                {"name": "native", "type": "openai", "api_key": "sk-up",
                 "base_url": "http://127.0.0.1:1/v1", "responses": "native"},
                {"name": "ok", "type": "openai"}
            ],
            "models": [
                {"alias": "all-native", "routing": {"strategy": "round_robin",
                    "targets": [{"provider": "native", "model_id": "up-v1"}]}},
                {"alias": "mixed", "routing": {"strategy": "failover",
                    "targets": [{"provider": "native", "model_id": "up-v1"}],
                    "fallback": [{"provider": "ok", "model_id": "ok-v1"}]}}
            ]
        }))
        .unwrap();
        let mut registry =
            crate::providers::build_registry(&config.providers[..1], &config.defaults)
                .await
                .unwrap();
        registry.insert(
            "ok".to_string(),
            Arc::new(OkProvider {
                seen: Mutex::new(None),
            }),
        );
        let router = crate::router::ModelRouter::from_config(&config, &registry).unwrap();
        let body = Bytes::from(json!({"model": "m", "input": "Hi"}).to_string());
        let req: ResponsesRequest = serde_json::from_slice(&body).unwrap();

        let plan = Plan::new(&router.resolve("all-native").unwrap(), &req, &body).unwrap();
        assert!(plan.native.is_some());
        assert!(
            plan.translated.is_none(),
            "nothing could use the translation"
        );

        let plan = Plan::new(&router.resolve("mixed").unwrap(), &req, &body).unwrap();
        assert!(plan.translated().is_some());
    }

    #[tokio::test]
    async fn native_aliases_still_reject_stateful_fields() {
        let alias = "resp-native-stateful";
        let upstream = mock_upstream(200).await;
        let h = native_harness(alias, native_only(), &upstream).await;

        for (extra, param) in [
            (
                json!({"previous_response_id": "resp_1"}),
                "previous_response_id",
            ),
            (json!({"store": true}), "store"),
            (json!({"background": true}), "background"),
        ] {
            let mut body = json!({"model": alias, "input": "Hi"});
            body.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let (status, _, resp) = post(&h.app, Some(KEY), &body.to_string()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {resp}");
            assert_eq!(error_body(&resp)["param"], param, "{resp}");
        }
        assert!(upstream.seen.lock().unwrap().is_empty());
    }
}
