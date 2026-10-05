//! End-to-end budget accounting through the real router and `auth_middleware`
//! (#186): every request that reserves budget settles it exactly once —
//! reconciled with actual usage on success, refunded in full on any other
//! exit — and the model listings reserve nothing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use tower::ServiceExt as _;
use uuid::Uuid;

use crate::budget_enforcer::{RecordingBudget, DEFAULT_RESERVE_TOKENS};
use crate::config::Config;
use crate::state::AppState;

const SECRET: &[u8] = b"test-secret-for-budget-refund-tests";
const KID: &str = "budget-kid";
const ISSUER: &str = "https://budget.test";
const USAGE: u32 = 15;

/// Mock OpenAI upstream: `/v1/chat/completions` answers with 10+5 tokens of
/// usage and `/v1/responses` (for `responses: native`) with 13+5, as SSE when
/// `stream` is set; anything under `/hang` never answers.
async fn spawn_upstream() -> String {
    async fn chat(axum::Json(body): axum::Json<serde_json::Value>) -> axum::response::Response {
        let usage =
            serde_json::json!({"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15});
        if body["stream"].as_bool() == Some(true) {
            let chunk = serde_json::json!({
                "id": "c", "object": "chat.completion.chunk", "created": 0, "model": "m",
                "choices": [{"index": 0, "delta": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
                "usage": usage,
            });
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                format!("data: {chunk}\n\ndata: [DONE]\n\n"),
            )
                .into_response()
        } else {
            axum::Json(serde_json::json!({
                "id": "c", "object": "chat.completion", "created": 0, "model": "m",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
                "usage": usage,
            }))
            .into_response()
        }
    }
    async fn native(axum::Json(body): axum::Json<serde_json::Value>) -> axum::response::Response {
        let usage = serde_json::json!({"input_tokens": 13, "output_tokens": 5, "total_tokens": 18});
        let response = serde_json::json!({
            "id": "resp_up", "object": "response", "status": "completed", "model": "m",
            "output": [], "usage": usage,
        });
        if body["stream"].as_bool() == Some(true) {
            let done = serde_json::json!({"type": "response.completed", "response": response});
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                format!("event: response.completed\ndata: {done}\n\n"),
            )
                .into_response()
        } else {
            axum::Json(response).into_response()
        }
    }
    async fn hang() -> &'static str {
        futures::future::pending::<()>().await;
        unreachable!()
    }
    let app = axum::Router::new()
        .route("/v1/chat/completions", axum::routing::post(chat))
        .route("/v1/responses", axum::routing::post(native))
        .route("/hang/v1/chat/completions", axum::routing::post(hang));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// A local port with nothing listening on it.
async fn dead_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

struct Gateway {
    app: axum::Router,
    budget: Arc<RecordingBudget>,
    client_id: Uuid,
}

impl Gateway {
    async fn new() -> Self {
        let upstream = spawn_upstream().await;
        let dead = format!("http://127.0.0.1:{}/v1", dead_port().await);
        let config: Config = serde_json::from_value(serde_json::json!({
            "defaults": {"retry": {"max_attempts": 1, "jitter": false}},
            "providers": [
                {"name": "mock", "type": "openai", "api_key": "k", "base_url": format!("{upstream}/v1")},
                {"name": "hang", "type": "openai", "api_key": "k", "base_url": format!("{upstream}/hang/v1")},
                {"name": "dead", "type": "openai", "api_key": "k", "base_url": dead},
                {"name": "native", "type": "openai", "api_key": "k", "base_url": format!("{upstream}/v1"), "responses": "native"},
                {"name": "native-dead", "type": "openai", "api_key": "k", "base_url": dead, "responses": "native"},
            ],
            "models": [
                {"alias": "ok", "routing": {"strategy": "failover", "targets": [{"provider": "mock", "model_id": "m"}]}},
                {"alias": "hang", "routing": {"strategy": "failover", "targets": [{"provider": "hang", "model_id": "m"}]}},
                {"alias": "down", "routing": {"strategy": "failover", "targets": [{"provider": "dead", "model_id": "m"}]}},
                {"alias": "native", "routing": {"strategy": "failover", "targets": [{"provider": "native", "model_id": "m"}]}},
                {"alias": "native-down", "routing": {"strategy": "failover", "targets": [{"provider": "native-dead", "model_id": "m"}]}},
            ],
            "trusted_issuers": [{"issuer": ISSUER, "jwks_uri": format!("{ISSUER}/jwks.json")}],
        }))
        .unwrap();

        let registry = crate::providers::build_registry(&config.providers, &config.defaults)
            .await
            .unwrap();
        let router = crate::router::ModelRouter::from_config(&config, &registry).unwrap();
        let resolver = crate::classifier::RouteResolver::build(&config, router).unwrap();
        let jwks_cache = crate::jwks::JwksCache::new(
            config.trusted_issuers.clone(),
            300,
            reqwest::Client::new(),
        );
        let jwks: jsonwebtoken::jwk::JwkSet = serde_json::from_value(serde_json::json!({
            "keys": [{"kty": "oct", "kid": KID, "alg": "HS256", "k": URL_SAFE_NO_PAD.encode(SECRET)}]
        }))
        .unwrap();
        jwks_cache
            .seed_for_test(ISSUER, &format!("{ISSUER}/jwks.json"), jwks)
            .await;

        let budget = Arc::new(RecordingBudget::default());
        let state = AppState {
            rate_limit_backend: Arc::new(crate::ratelimit::MemoryBackend::new()),
            resolver: Arc::new(resolver),
            providers: Arc::new(registry),
            metrics: Arc::new(crate::metrics::Metrics::new()),
            ready: Arc::new(AtomicBool::new(true)),
            jwks_cache: Arc::new(jwks_cache),
            config: Arc::new(config),
            usage_writer: crate::usage_writer::noop_writer(),
            budget_enforcer: budget.clone(),
            event_dispatcher: crate::event_dispatcher::noop_dispatcher(),
        };
        Self {
            app: crate::server::build_router(state),
            budget,
            client_id: Uuid::new_v4(),
        }
    }

    /// A budgeted JWT allowed to use the configured aliases and the
    /// unconfigured `ghost` (for the 404) — but not `other` (for the 403).
    fn token(&self) -> String {
        let claims = serde_json::json!({
            "sub": "svc", "iss": ISSUER, "exp": 9_999_999_999u64,
            "ferrox": {
                "client_id": self.client_id.to_string(),
                "allowed_models": ["ok", "hang", "down", "native", "native-down", "ghost"],
                "token_budget": 1_000_000,
                "budget_period": "daily",
            },
        });
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        header.kid = Some(KID.to_string());
        jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(SECRET),
        )
        .unwrap()
    }

    fn request(&self, method: &str, path: &str, body: impl Into<Body>) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {}", self.token()))
            .header("content-type", "application/json")
            .body(body.into())
            .unwrap()
    }

    /// Send a request and read the whole body, so a streamed response has
    /// finished (and settled) before this returns.
    async fn send(&self, method: &str, path: &str, body: impl Into<Body>) -> StatusCode {
        let resp = self
            .app
            .clone()
            .oneshot(self.request(method, path, body))
            .await
            .unwrap();
        let status = resp.status();
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        status
    }

    /// The reconciliations recorded once any spawned from `Drop` have run.
    async fn settled(&self) -> Vec<(String, String, u32, u32)> {
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        self.budget.reconciles()
    }

    fn once(&self, actual: u32) -> Vec<(String, String, u32, u32)> {
        vec![(
            self.client_id.to_string(),
            "daily".to_string(),
            DEFAULT_RESERVE_TOKENS,
            actual,
        )]
    }
}

#[derive(Clone, Copy, Debug)]
enum Api {
    Chat,
    Anthropic,
    Responses,
}

const APIS: [Api; 3] = [Api::Chat, Api::Anthropic, Api::Responses];

impl Api {
    fn path(self) -> &'static str {
        match self {
            Api::Chat => "/v1/chat/completions",
            Api::Anthropic => "/anthropic/v1/messages",
            Api::Responses => "/v1/responses",
        }
    }

    fn body(self, model: &str, stream: bool) -> String {
        let body = match self {
            Api::Chat => serde_json::json!({
                "model": model, "stream": stream,
                "messages": [{"role": "user", "content": "hi"}],
            }),
            Api::Anthropic => serde_json::json!({
                "model": model, "stream": stream, "max_tokens": 16,
                "messages": [{"role": "user", "content": "hi"}],
            }),
            Api::Responses => serde_json::json!({"model": model, "stream": stream, "input": "hi"}),
        };
        body.to_string()
    }

    /// A body this surface rejects before dispatch.
    fn invalid_body(self) -> String {
        match self {
            // Chat's `Json` extractor rejects it before the handler runs;
            // the Anthropic handler rejects it in its own parse.
            Api::Chat | Api::Anthropic => "{not json".to_string(),
            // Parses, then fails translation: hosted tools are unsupported.
            Api::Responses => serde_json::json!({
                "model": "ok", "input": "hi", "tools": [{"type": "web_search"}],
            })
            .to_string(),
        }
    }
}

#[tokio::test]
async fn pre_dispatch_failures_refund_the_reservation_once() {
    let cases = [
        ("invalid body", None, None),
        ("403", Some("other"), Some(StatusCode::FORBIDDEN)),
        ("404", Some("ghost"), Some(StatusCode::NOT_FOUND)),
    ];
    for api in APIS {
        for (label, model, want) in cases {
            let gw = Gateway::new().await;
            let body = match model {
                Some(m) => api.body(m, false),
                None => api.invalid_body(),
            };
            let status = gw.send("POST", api.path(), body).await;
            match want {
                Some(want) => assert_eq!(status, want, "{api:?} {label}"),
                // An unparsable body is rejected, but the Anthropic surface
                // maps it to a 500, so only the rejection is asserted.
                None => assert!(!status.is_success(), "{api:?} {label}: {status}"),
            }
            assert_eq!(gw.settled().await, gw.once(0), "{api:?} {label}");
            assert_eq!(gw.budget.reserves.load(Ordering::Relaxed), 1);
        }
    }
}

#[tokio::test]
async fn dispatch_failure_refunds_the_reservation_once() {
    for api in APIS {
        for stream in [false, true] {
            let gw = Gateway::new().await;
            let status = gw.send("POST", api.path(), api.body("down", stream)).await;
            assert!(
                status.is_server_error(),
                "{api:?} stream={stream}: {status}"
            );
            assert_eq!(gw.settled().await, gw.once(0), "{api:?} stream={stream}");
        }
    }
}

#[tokio::test]
async fn success_reconciles_actual_usage_once() {
    for api in APIS {
        for stream in [false, true] {
            let gw = Gateway::new().await;
            let status = gw.send("POST", api.path(), api.body("ok", stream)).await;
            assert_eq!(status, StatusCode::OK, "{api:?} stream={stream}");
            assert_eq!(
                gw.settled().await,
                gw.once(USAGE),
                "{api:?} stream={stream}"
            );
        }
    }
}

#[tokio::test]
async fn dropping_the_request_mid_dispatch_refunds_the_reservation() {
    for api in APIS {
        let gw = Gateway::new().await;
        let pending =
            gw.app
                .clone()
                .oneshot(gw.request("POST", api.path(), api.body("hang", false)));
        // The upstream never answers: the client gives up and the handler
        // future is dropped mid-dispatch, before any finalizer exists.
        assert!(tokio::time::timeout(Duration::from_millis(200), pending)
            .await
            .is_err());
        assert_eq!(gw.settled().await, gw.once(0), "{api:?}");
    }
}

#[tokio::test]
async fn native_responses_passthrough_settles_once() {
    // A hosted tool only a `responses: native` target can serve.
    let body = |model: &str, stream: bool| {
        serde_json::json!({
            "model": model, "stream": stream, "input": "hi",
            "tools": [{"type": "web_search"}],
        })
        .to_string()
    };
    for stream in [false, true] {
        let gw = Gateway::new().await;
        let status = gw
            .send("POST", "/v1/responses", body("native", stream))
            .await;
        assert_eq!(status, StatusCode::OK, "stream={stream}");
        assert_eq!(gw.settled().await, gw.once(18), "stream={stream}");

        let gw = Gateway::new().await;
        let status = gw
            .send("POST", "/v1/responses", body("native-down", stream))
            .await;
        assert!(status.is_server_error(), "stream={stream}: {status}");
        assert_eq!(gw.settled().await, gw.once(0), "stream={stream}");
    }
}

#[tokio::test]
async fn model_listings_reserve_nothing() {
    let gw = Gateway::new().await;
    for path in ["/v1/models", "/anthropic/v1/models"] {
        for method in ["GET", "HEAD"] {
            assert_eq!(
                gw.send(method, path, Body::empty()).await,
                StatusCode::OK,
                "{method} {path}"
            );
        }
    }
    assert_eq!(gw.budget.reserves.load(Ordering::Relaxed), 0);
    assert!(gw.settled().await.is_empty());
}
