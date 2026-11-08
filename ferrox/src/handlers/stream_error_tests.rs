//! An upstream that fails *after* it answered 200 and started streaming,
//! through the real router, `auth_middleware` and OpenAI adapter, on all three
//! inbound surfaces: the client's body is read to completion and ends with the
//! surface's own terminal error event, instead of being cut off; a normal
//! stream is unaffected; and the failure is logged once at WARN and counted.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use tower::ServiceExt as _;

use crate::config::Config;
use crate::state::AppState;
use crate::telemetry::metrics::ERRORS_TOTAL;

const KEY: &str = "sk-stream-error-tests";

/// The upstream's one ordinary chunk.
const CHUNK: &str = r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":"hi"},"finish_reason":null}]}"#;

/// The in-band error frame the cc-openai shim sends when it fails part-way.
const SHIM_ERROR: &str = r#"{"error":{"message":"Claude stopped after reaching the maximum number of turns without producing a reply","type":"server_error","code":null}}"#;
const SHIM_MESSAGE: &str =
    "Claude stopped after reaching the maximum number of turns without producing a reply";

/// The final chunk of a normal stream, with usage.
const LAST_CHUNK: &str = r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}"#;

fn sse(body: String) -> axum::response::Response {
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        body,
    )
        .into_response()
}

/// Mock OpenAI-compatible upstreams, one per path prefix:
/// - `/shim`: a chunk, the shim's error frame, `[DONE]` — the exact bytes
///   from the production report;
/// - `/cut`: a chunk, then a frame that is not JSON;
/// - `/ok`: a normal stream;
/// - `/native-shim/v1/responses`: a native Responses stream that fails with
///   an unnamed chat-style error frame.
async fn spawn_upstream() -> String {
    let app = axum::Router::new()
        .route(
            "/shim/v1/chat/completions",
            axum::routing::post(|| async {
                sse(format!(
                    "data: {CHUNK}\n\ndata: {SHIM_ERROR}\n\ndata: [DONE]\n\n"
                ))
            }),
        )
        .route(
            "/cut/v1/chat/completions",
            axum::routing::post(|| async { sse(format!("data: {CHUNK}\n\ndata: {{not json\n\n")) }),
        )
        .route(
            "/ok/v1/chat/completions",
            axum::routing::post(|| async {
                sse(format!(
                    "data: {CHUNK}\n\ndata: {LAST_CHUNK}\n\ndata: [DONE]\n\n"
                ))
            }),
        )
        .route(
            "/native-shim/v1/responses",
            axum::routing::post(|| async {
                sse(format!(
                    "event: response.created\ndata: {}\n\ndata: {SHIM_ERROR}\n\n",
                    r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_up","object":"response","status":"in_progress","output":[]}}"#
                ))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn gateway() -> axum::Router {
    let upstream = spawn_upstream().await;
    let provider = |name: &str, prefix: &str, native: bool| {
        let mut p = serde_json::json!({"name": name, "type": "openai", "api_key": "k",
                                       "base_url": format!("{upstream}/{prefix}/v1")});
        if native {
            p["responses"] = "native".into();
        }
        p
    };
    let alias = |alias: &str, provider: &str| {
        serde_json::json!({"alias": alias, "routing": {"strategy": "failover",
                           "targets": [{"provider": provider, "model_id": "m"}]}})
    };
    let config: Config = serde_json::from_value(serde_json::json!({
        "defaults": {"retry": {"max_attempts": 1, "jitter": false}},
        "providers": [
            provider("se-shim-chat", "shim", false),
            provider("se-shim-anthropic", "shim", false),
            provider("se-shim-responses", "shim", false),
            provider("se-cut", "cut", false),
            provider("se-ok", "ok", false),
            provider("se-native-shim", "native-shim", true),
        ],
        "models": [
            alias("shim-chat", "se-shim-chat"),
            alias("shim-anthropic", "se-shim-anthropic"),
            alias("shim-responses", "se-shim-responses"),
            alias("cut", "se-cut"),
            alias("ok", "se-ok"),
            alias("native-shim", "se-native-shim"),
        ],
        "virtual_keys": [{"key": KEY, "name": "stream-error-tests", "allowed_models": ["*"]}],
    }))
    .unwrap();

    let registry = crate::providers::build_registry(&config.providers, &config.defaults)
        .await
        .unwrap();
    let router = crate::router::ModelRouter::from_config(&config, &registry).unwrap();
    let resolver = crate::classifier::RouteResolver::build(&config, router).unwrap();
    let state = AppState {
        rate_limit_backend: Arc::new(crate::ratelimit::MemoryBackend::new()),
        resolver: Arc::new(resolver),
        providers: Arc::new(registry),
        metrics: Arc::new(crate::metrics::Metrics::new()),
        ready: Arc::new(AtomicBool::new(true)),
        jwks_cache: Arc::new(crate::jwks::JwksCache::new(
            vec![],
            300,
            reqwest::Client::new(),
        )),
        config: Arc::new(config),
        usage_writer: crate::usage_writer::noop_writer(),
        budget_enforcer: Arc::new(crate::budget_enforcer::NoopBudgetEnforcer),
        event_dispatcher: crate::event_dispatcher::noop_dispatcher(),
    };
    crate::server::build_router(state)
}

/// POST `body` to `path` and read the whole response body. Panics if the body
/// errors part-way — the bug under test, which a client sees as a truncated
/// chunked body.
async fn post(path: &str, body: serde_json::Value) -> (StatusCode, String) {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", format!("Bearer {KEY}"))
        .header("content-type", "application/json")
        .header("x-request-id", "req-stream-error-test")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = gateway().await.oneshot(request).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("the body is read to completion, not aborted");
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

fn chat(model: &str) -> serde_json::Value {
    serde_json::json!({"model": model, "stream": true,
                       "messages": [{"role": "user", "content": "hi"}]})
}

/// The `data:` payloads of an SSE body, in order.
fn data_frames(body: &str) -> Vec<&str> {
    body.split("\n\n")
        .filter_map(|frame| frame.lines().find_map(|line| line.strip_prefix("data: ")))
        .collect()
}

/// `(event, data)` of every named SSE event in a body, in order.
fn named_events(body: &str) -> Vec<(&str, serde_json::Value)> {
    body.split("\n\n")
        .filter_map(|frame| {
            let event = frame.lines().find_map(|l| l.strip_prefix("event: "))?;
            let data = frame.lines().find_map(|l| l.strip_prefix("data: "))?;
            Some((event, serde_json::from_str(data).unwrap()))
        })
        .collect()
}

fn errors_total(provider: &str, error_type: &str) -> f64 {
    ERRORS_TOTAL
        .with_label_values(&[provider, error_type])
        .get()
}

// ── /v1/chat/completions ─────────────────────────────────────────────────────

#[tokio::test]
#[tracing_test::traced_test]
async fn chat_stream_forwards_the_upstream_error_frame_then_done() {
    let (status, body) = post("/v1/chat/completions", chat("shim-chat")).await;
    assert_eq!(status, StatusCode::OK);

    let frames = data_frames(&body);
    assert_eq!(frames.len(), 3, "chunk, error, [DONE]: {body}");
    let chunk: serde_json::Value = serde_json::from_str(frames[0]).unwrap();
    assert_eq!(chunk["choices"][0]["delta"]["content"], "hi");
    assert_eq!(
        frames[1], SHIM_ERROR,
        "the upstream's OpenAI error object, forwarded byte for byte"
    );
    assert_eq!(frames[2], "[DONE]");
    assert!(body.ends_with("data: [DONE]\n\n"), "{body:?}");

    // One WARN, with what is needed to diagnose it.
    assert!(logs_contain("WARN"));
    assert!(logs_contain("upstream failed mid-stream"));
    assert!(logs_contain("request_id=req-stream-error-test"));
    assert!(logs_contain("model_alias=shim-chat"));
    assert!(logs_contain("provider=se-shim-chat"));
    assert!(logs_contain("model_id=m"));
    assert!(logs_contain("upstream_status=502"));
    assert!(logs_contain("chunks_forwarded=1"));
    assert!(logs_contain(SHIM_MESSAGE));
    logs_assert(|lines| {
        match lines
            .iter()
            .filter(|l| l.contains("upstream failed mid-stream"))
            .count()
        {
            1 => Ok(()),
            n => Err(format!("expected one WARN, got {n}")),
        }
    });
    assert_eq!(errors_total("se-shim-chat", "provider_error"), 1.0);
}

#[tokio::test]
async fn chat_stream_malformed_frame_ends_with_a_gateway_error_then_done() {
    let (status, body) = post("/v1/chat/completions", chat("cut")).await;
    assert_eq!(status, StatusCode::OK);

    let frames = data_frames(&body);
    assert_eq!(frames.len(), 3, "chunk, error, [DONE]: {body}");
    // Not from the upstream: the shape of the non-streaming error body.
    let error: serde_json::Value = serde_json::from_str(frames[1]).unwrap();
    assert_eq!(error["error"]["type"], "stream_error");
    assert_eq!(error["error"]["code"], 500);
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Failed to parse OpenAI chunk"));
    assert_eq!(frames[2], "[DONE]");
    assert_eq!(errors_total("se-cut", "stream_error"), 1.0);
}

#[tokio::test]
async fn normal_chat_stream_is_unchanged() {
    let (status, body) = post("/v1/chat/completions", chat("ok")).await;
    assert_eq!(status, StatusCode::OK);
    // Each chunk is the adapter's parse of the upstream's, re-serialized —
    // exactly as before; nothing is added.
    let reserialize = |raw: &str| {
        serde_json::to_string(
            &serde_json::from_str::<crate::types::ChatCompletionChunk>(raw).unwrap(),
        )
        .unwrap()
    };
    assert_eq!(
        body,
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            reserialize(CHUNK),
            reserialize(LAST_CHUNK)
        )
    );
    assert_eq!(errors_total("se-ok", "provider_error"), 0.0);
    assert_eq!(errors_total("se-ok", "stream_error"), 0.0);
}

// ── /anthropic/v1/messages ───────────────────────────────────────────────────

#[tokio::test]
async fn anthropic_stream_ends_with_an_error_event_and_no_message_stop() {
    let (status, body) = post(
        "/anthropic/v1/messages",
        serde_json::json!({"model": "shim-anthropic", "stream": true, "max_tokens": 16,
                           "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let events = named_events(&body);
    let names: Vec<&str> = events.iter().map(|(name, _)| *name).collect();
    assert_eq!(names.first(), Some(&"message_start"));
    assert!(!names.contains(&"message_stop"), "{names:?}");
    let (name, data) = events.last().unwrap();
    assert_eq!(*name, "error");
    assert_eq!(
        *data,
        serde_json::json!({"type": "error",
                           "error": {"type": "api_error", "message": SHIM_MESSAGE}})
    );
    assert_eq!(errors_total("se-shim-anthropic", "provider_error"), 1.0);
}

// ── /v1/responses ────────────────────────────────────────────────────────────

#[tokio::test]
async fn translated_responses_stream_ends_with_response_failed() {
    let (status, body) = post(
        "/v1/responses",
        serde_json::json!({"model": "shim-responses", "stream": true, "input": "hi"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let events = named_events(&body);
    let (name, data) = events.last().unwrap();
    assert_eq!(*name, "response.failed", "{body}");
    assert_eq!(data["type"], "response.failed");
    assert_eq!(data["response"]["status"], "failed");
    assert_eq!(data["response"]["error"]["message"], SHIM_MESSAGE);
    // Sequence numbers stay contiguous through the failure.
    let seqs: Vec<u64> = events
        .iter()
        .map(|(_, d)| d["sequence_number"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, (0..seqs.len() as u64).collect::<Vec<_>>());
    assert_eq!(errors_total("se-shim-responses", "provider_error"), 1.0);
}

#[tokio::test]
async fn native_responses_stream_ends_with_an_error_event() {
    let (status, body) = post(
        "/v1/responses",
        serde_json::json!({"model": "native-shim", "stream": true, "store": false,
                           "input": "hi"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let events = named_events(&body);
    assert_eq!(events.len(), 2, "{body}");
    assert_eq!(events[0].0, "response.created");
    let (name, data) = &events[1];
    assert_eq!(*name, "error");
    assert_eq!(
        *data,
        // `code` is the upstream's own `error.type`.
        serde_json::json!({"type": "error", "code": "server_error",
                           "message": SHIM_MESSAGE, "param": null,
                           "sequence_number": 1})
    );
    assert_eq!(errors_total("se-native-shim", "provider_error"), 1.0);
}
