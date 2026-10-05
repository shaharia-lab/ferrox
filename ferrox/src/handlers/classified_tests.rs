//! Classified aliases through the real router and `auth_middleware`, on all
//! three inbound surfaces (#193): the request is served by the pool the
//! classifier chose, or by the fallback pool when the classifier fails;
//! authorization is checked against the requested alias, before the
//! classifier; and everything recorded about the request names the alias
//! that served it. An alias in shadow mode, and a request opting out with
//! `x-ferrox-classifier: skip`, are served by the fallback pool (#196).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tower::ServiceExt as _;

use crate::classifier::test_support::{
    gateway_config, registry_with, shadowed, Answer, FakeClassifier, Upstream,
};
use crate::classifier::{ClassifierError, RouteResolver};
use crate::config::Config;
use crate::state::AppState;
use crate::usage_writer::{UsageEvent, UsageWriter};

struct Gateway {
    app: axum::Router,
    classifier: Arc<FakeClassifier>,
    usage_rx: mpsc::Receiver<UsageEvent>,
}

/// The gateway of [`gateway_config`], its classifier answering `reply`.
fn gateway(reply: Answer) -> Gateway {
    gateway_for(gateway_config(json!({})), reply)
}

/// The gateway of `config`, its classifier answering `reply`.
fn gateway_for(config: Config, reply: Answer) -> Gateway {
    let registry = Upstream::registry();
    let classifier = FakeClassifier::new(reply);
    let router = crate::router::ModelRouter::from_config(&config, &registry).unwrap();
    let resolver =
        RouteResolver::from_config(&config, router, &registry_with(classifier.clone())).unwrap();
    let (usage_writer, usage_rx) = UsageWriter::channel(16);
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
        event_dispatcher: crate::event_dispatcher::noop_dispatcher(),
    };
    Gateway {
        app: crate::server::build_router(state),
        classifier,
        usage_rx,
    }
}

/// An inbound surface, and how to read the `model` it reports back.
struct Surface {
    path: &'static str,
    /// A request for `model` asking "Hi".
    body: fn(model: &str, stream: bool) -> Value,
    /// `model` of a non-streaming answer served by `provider`.
    model: fn(provider: &str) -> String,
    /// `model` of a streamed answer served by `provider`.
    stream_model: fn(provider: &str) -> String,
}

/// What each surface puts in the response's `model` for a request to the
/// classified alias `auto`. The classifier changes none of this: each surface
/// reports what it reports for a statically routed alias — the upstream's
/// model where the provider's answer is passed on, the requested alias where
/// the gateway writes the envelope itself.
const SURFACES: [Surface; 3] = [
    Surface {
        path: "/v1/chat/completions",
        body: |model, stream| {
            json!({"model": model, "stream": stream,
                   "messages": [{"role": "user", "content": "Hi"}]})
        },
        model: |provider| format!("{provider}-model"),
        stream_model: |provider| format!("{provider}-model"),
    },
    Surface {
        path: "/v1/responses",
        body: |model, stream| json!({"model": model, "stream": stream, "input": "Hi"}),
        model: |_| "auto".to_string(),
        stream_model: |_| "auto".to_string(),
    },
    Surface {
        path: "/anthropic/v1/messages",
        body: |model, stream| {
            json!({"model": model, "stream": stream, "max_tokens": 16,
                   "messages": [{"role": "user", "content": [{"type": "text", "text": "Hi"}]}]})
        },
        model: |provider| format!("{provider}-model"),
        stream_model: |_| "auto".to_string(),
    },
];

async fn post(app: &axum::Router, path: &str, key: &str, body: &Value) -> (StatusCode, String) {
    post_with(app, path, key, None, body).await
}

/// [`post`], with the `x-ferrox-classifier` header set to `classifier`.
async fn post_with(
    app: &axum::Router,
    path: &str,
    key: &str,
    classifier: Option<&str>,
    body: &Value,
) -> (StatusCode, String) {
    let (status, _, body) = send(app, path, key, classifier, body).await;
    (status, body)
}

/// [`post_with`], with the response headers.
async fn send(
    app: &axum::Router,
    path: &str,
    key: &str,
    classifier: Option<&str>,
    body: &Value,
) -> (StatusCode, HeaderMap, String) {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json");
    if let Some(value) = classifier {
        request = request.header("x-ferrox-classifier", value);
    }
    let request = request.body(Body::from(body.to_string())).unwrap();
    let resp = app.clone().oneshot(request).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}

fn routed_model(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("x-ferrox-routed-model")
        .map(|v| v.to_str().unwrap())
}

/// The first `model` an answer reports: the JSON body's, or that of the
/// first SSE frame carrying one (at the top level, or inside the `message` /
/// `response` envelope of the Anthropic and Responses start events).
fn reported_model(body: &str, stream: bool) -> Option<String> {
    let model_of = |value: &Value| {
        [
            &value["model"],
            &value["message"]["model"],
            &value["response"]["model"],
        ]
        .into_iter()
        .find_map(|model| model.as_str().map(str::to_string))
    };
    if !stream {
        return model_of(&serde_json::from_str(body).unwrap());
    }
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .find_map(|frame| model_of(&frame))
}

/// Request `auto` on every surface, streaming and not, and check which alias
/// and provider served it.
async fn assert_auto_is_served_by(reply: Answer, alias: &str, provider: &str, reason: &str) {
    for surface in &SURFACES {
        for stream in [false, true] {
            let case = format!("{} stream={stream} {reply:?}", surface.path);
            let mut gateway = gateway(reply.clone());

            let (status, headers, body) = send(
                &gateway.app,
                surface.path,
                "sk-all",
                None,
                &(surface.body)("auto", stream),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{case}: {body}");
            assert_eq!(gateway.classifier.calls(), 1, "{case}");
            // The client is told which alias answered.
            assert_eq!(routed_model(&headers), Some(alias), "{case}");

            // The classifier saw the conversation, whatever the surface.
            let (input, _) = gateway.classifier.seen().expect(&case);
            assert_eq!(input.turns.len(), 1, "{case}");
            assert_eq!(input.turns[0].text, "Hi", "{case}");

            // Usage is recorded under the alias that served the request.
            let usage = gateway.usage_rx.recv().await.expect(&case);
            assert_eq!(usage.model, alias, "{case}");
            assert_eq!(usage.provider, provider, "{case}");
            // ... with the alias that was asked for, and why it went there.
            assert_eq!(usage.requested_model.as_deref(), Some("auto"), "{case}");
            assert_eq!(usage.routing_reason, Some(reason), "{case}");
            assert!(usage.classifier_latency_ms.is_some(), "{case}");
            let answered = matches!(reply, Answer::Tier(..));
            assert_eq!(usage.classifier_confidence.is_some(), answered, "{case}");
            assert_eq!(
                usage.classifier_input_tokens,
                answered.then_some(3),
                "{case}"
            );
            assert_eq!(
                usage.classifier_model.as_deref(),
                answered.then_some("fake-1"),
                "{case}"
            );

            let expected = if stream {
                (surface.stream_model)(provider)
            } else {
                (surface.model)(provider)
            };
            assert_eq!(reported_model(&body, stream), Some(expected), "{case}");
        }
    }
}

#[tokio::test]
async fn a_classified_alias_is_served_by_the_chosen_tiers_pool() {
    assert_auto_is_served_by(
        Answer::Tier("simple", Some(0.9)),
        "fast",
        "fast-up",
        "classified",
    )
    .await;
}

#[tokio::test]
async fn a_failing_classifier_serves_the_fallback_pool() {
    let failed = ClassifierError::Failed("upstream said 529".to_string());
    assert_auto_is_served_by(Answer::Fail(failed), "smart", "smart-up", "error").await;
}

/// `allowed_models` is checked against the requested alias only: a key
/// granted `auto` is served by whichever tier the classifier picks, though
/// it could not ask for that tier's alias by name.
#[tokio::test]
async fn a_key_allowed_only_the_classified_alias_is_served() {
    for surface in &SURFACES {
        let mut gateway = gateway(Answer::Tier("simple", Some(0.9)));

        let (status, body) = post(
            &gateway.app,
            surface.path,
            "sk-auto",
            &(surface.body)("auto", false),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}: {body}", surface.path);
        assert_eq!(gateway.usage_rx.recv().await.unwrap().model, "fast");

        let (status, _) = post(
            &gateway.app,
            surface.path,
            "sk-auto",
            &(surface.body)("fast", false),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{}", surface.path);
    }
}

#[tokio::test]
async fn a_key_not_allowed_the_classified_alias_is_refused_before_the_classifier() {
    for surface in &SURFACES {
        let mut gateway = gateway(Answer::Tier("simple", Some(0.9)));

        let (status, body) = post(
            &gateway.app,
            surface.path,
            "sk-fast",
            &(surface.body)("auto", false),
        )
        .await;

        assert_eq!(status, StatusCode::FORBIDDEN, "{}: {body}", surface.path);
        assert!(
            body.contains("not authorized to use model 'auto'"),
            "{body}"
        );
        assert_eq!(gateway.classifier.calls(), 0, "{}", surface.path);
        assert!(gateway.usage_rx.try_recv().is_err(), "{}", surface.path);
    }
}

/// A statically routed alias answers exactly as it did before classifiers:
/// no routed-model header, no routing decision in its usage.
#[tokio::test]
async fn a_static_alias_never_reaches_the_classifier() {
    for surface in &SURFACES {
        for stream in [false, true] {
            let case = format!("{} stream={stream}", surface.path);
            let mut gateway = gateway(Answer::Tier("complex", Some(0.9)));

            let (status, headers, body) = send(
                &gateway.app,
                surface.path,
                "sk-all",
                None,
                &(surface.body)("fast", stream),
            )
            .await;

            assert_eq!(status, StatusCode::OK, "{case}: {body}");
            assert_eq!(routed_model(&headers), None, "{case}");
            assert_eq!(gateway.classifier.calls(), 0, "{case}");
            let usage = gateway.usage_rx.recv().await.unwrap();
            assert_eq!(
                (usage.model.as_str(), usage.provider.as_str()),
                ("fast", "fast-up"),
                "{case}"
            );
            assert_eq!(usage.requested_model, None, "{case}");
            assert_eq!(usage.routing_reason, None, "{case}");
            assert_eq!(usage.classifier_confidence, None, "{case}");
            assert_eq!(usage.classifier_latency_ms, None, "{case}");
            assert_eq!(usage.classifier_input_tokens, None, "{case}");
            assert_eq!(usage.classifier_model, None, "{case}");
        }
    }
}

/// The header reports a served request: a refused one carries none.
#[tokio::test]
async fn an_error_response_carries_no_routed_model_header() {
    for surface in &SURFACES {
        let gateway = gateway(Answer::Tier("simple", Some(0.9)));
        let (status, headers, _) = send(
            &gateway.app,
            surface.path,
            "sk-fast",
            None,
            &(surface.body)("auto", false),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{}", surface.path);
        assert_eq!(routed_model(&headers), None, "{}", surface.path);
    }
}

/// Shadow mode: the classifier is asked, and would have chosen `fast`, but
/// the fallback pool serves the request on every surface.
#[tokio::test]
async fn a_shadowed_alias_is_classified_and_served_by_the_fallback_pool() {
    for surface in &SURFACES {
        for stream in [false, true] {
            let case = format!("{} stream={stream}", surface.path);
            let config = shadowed(gateway_config(json!({})));
            let mut gateway = gateway_for(config, Answer::Tier("simple", Some(0.9)));

            let body = (surface.body)("auto", stream);
            let (status, body) = post(&gateway.app, surface.path, "sk-all", &body).await;

            assert_eq!(status, StatusCode::OK, "{case}: {body}");
            assert_eq!(gateway.classifier.calls(), 1, "{case}");
            let usage = gateway.usage_rx.recv().await.expect(&case);
            assert_eq!(usage.model, "smart", "{case}");
            assert_eq!(usage.provider, "smart-up", "{case}");
        }
    }
}

/// A failing classifier in shadow mode is the ordinary fallback.
#[tokio::test]
async fn a_shadowed_alias_with_a_failing_classifier_serves_the_fallback_pool() {
    for surface in &SURFACES {
        let config = shadowed(gateway_config(json!({})));
        let mut gateway = gateway_for(config, Answer::Fail(ClassifierError::Timeout));

        let body = (surface.body)("auto", false);
        let (status, body) = post(&gateway.app, surface.path, "sk-all", &body).await;

        assert_eq!(status, StatusCode::OK, "{}: {body}", surface.path);
        assert_eq!(gateway.classifier.calls(), 1, "{}", surface.path);
        assert_eq!(gateway.usage_rx.recv().await.unwrap().model, "smart");
    }
}

/// `x-ferrox-classifier: skip`: the classifier, which would have chosen
/// `fast`, is not called, and the fallback pool serves the request.
#[tokio::test]
async fn an_opted_out_request_is_served_by_the_fallback_pool_without_the_classifier() {
    for surface in &SURFACES {
        for (stream, value) in [(false, "skip"), (true, "SKIP")] {
            let case = format!("{} stream={stream}", surface.path);
            let mut gateway = gateway(Answer::Tier("simple", Some(0.9)));

            let body = (surface.body)("auto", stream);
            let (status, body) =
                post_with(&gateway.app, surface.path, "sk-all", Some(value), &body).await;

            assert_eq!(status, StatusCode::OK, "{case}: {body}");
            assert_eq!(gateway.classifier.calls(), 0, "{case}");
            let usage = gateway.usage_rx.recv().await.expect(&case);
            assert_eq!(usage.model, "smart", "{case}");
            assert_eq!(usage.provider, "smart-up", "{case}");
        }
    }
}

/// Any other value of the header is ignored: the request is classified.
#[tokio::test]
async fn another_value_of_the_opt_out_header_changes_nothing() {
    for surface in &SURFACES {
        let mut gateway = gateway(Answer::Tier("simple", Some(0.9)));

        let body = (surface.body)("auto", false);
        let (status, body) =
            post_with(&gateway.app, surface.path, "sk-all", Some("fast"), &body).await;

        assert_eq!(status, StatusCode::OK, "{}: {body}", surface.path);
        assert_eq!(gateway.classifier.calls(), 1, "{}", surface.path);
        assert_eq!(gateway.usage_rx.recv().await.unwrap().model, "fast");
    }
}

/// On a statically routed alias the header has no effect: the same pool
/// serves the request, with the same answer, as without it.
#[tokio::test]
async fn the_opt_out_header_has_no_effect_on_a_static_alias() {
    for surface in &SURFACES {
        let mut answers = Vec::new();
        for header in [None, Some("skip")] {
            let mut gateway = gateway(Answer::Tier("complex", Some(0.9)));

            let body = (surface.body)("fast", false);
            let (status, body) =
                post_with(&gateway.app, surface.path, "sk-all", header, &body).await;

            assert_eq!(status, StatusCode::OK, "{}: {body}", surface.path);
            assert_eq!(gateway.classifier.calls(), 0, "{}", surface.path);
            let usage = gateway.usage_rx.recv().await.unwrap();
            assert_eq!(
                (usage.model.as_str(), usage.provider.as_str()),
                ("fast", "fast-up"),
                "{}",
                surface.path
            );
            answers.push(reported_model(&body, false));
        }
        assert_eq!(answers[0], answers[1], "{}", surface.path);
    }
}

/// Neither mode widens what a key may ask for: `allowed_models` is still
/// checked against the requested alias, before anything else.
#[tokio::test]
async fn allowed_models_is_checked_against_the_requested_alias_in_both_modes() {
    for surface in &SURFACES {
        for (shadow, header) in [(true, None), (false, Some("skip"))] {
            let case = format!("{} shadow={shadow} header={header:?}", surface.path);
            let mut config = gateway_config(json!({}));
            if shadow {
                config = shadowed(config);
            }
            let mut gateway = gateway_for(config, Answer::Tier("simple", Some(0.9)));
            let body = (surface.body)("auto", false);

            // Not allowed `auto`: refused, and the classifier is not called.
            let (status, _) = post_with(&gateway.app, surface.path, "sk-fast", header, &body).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{case}");
            assert_eq!(gateway.classifier.calls(), 0, "{case}");
            assert!(gateway.usage_rx.try_recv().is_err(), "{case}");

            // Allowed only `auto`: served by its fallback, which the key
            // could not ask for by name.
            let (status, body) =
                post_with(&gateway.app, surface.path, "sk-auto", header, &body).await;
            assert_eq!(status, StatusCode::OK, "{case}: {body}");
            assert_eq!(
                gateway.usage_rx.recv().await.unwrap().model,
                "smart",
                "{case}"
            );
        }
    }
}
