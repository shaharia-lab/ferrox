//! The Jev backend against a mock System One server, on its own and behind
//! the resolver's time limit and circuit breaker.

use std::time::Duration;

use serde_json::{json, Value};
use tracing_test::traced_test;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::input::{Role, Turn};
use super::jev::JevClassifier;
use super::resolver::Reason;
use super::test_support::{gateway_config, Upstream};
use super::{Classification, Classifier, ClassifierError, ClassifierInput, RouteResolver, Tier};
use crate::config::ClassifierConfig;
use crate::router::ModelRouter;

/// Text that must never leave the gateway except in the request to Jev.
const USER_TEXT: &str = "my-private-question";

fn input(_max_input_chars: usize) -> ClassifierInput {
    let turn = |role, text: &str| Turn {
        role,
        text: text.to_string(),
    };
    ClassifierInput {
        turns: vec![
            turn(Role::User, "Hi"),
            turn(Role::Assistant, "Hello, how can I help?"),
            turn(Role::User, USER_TEXT),
        ],
    }
}

/// The tiers of [`gateway_config`]'s `auto` alias.
fn tiers() -> Vec<Tier> {
    let tier = |name: &str, when: &str| Tier {
        name: name.to_string(),
        when: when.to_string(),
    };
    vec![
        tier("complex", "Multi-step reasoning or code"),
        tier("simple", "Short factual or chat"),
    ]
}

fn config(base_url: &str) -> ClassifierConfig {
    serde_json::from_value(json!({
        "id": "jev-main", "type": "jev", "api_key": "ts-secret", "base_url": base_url
    }))
    .unwrap()
}

fn jev(server: &MockServer) -> JevClassifier {
    JevClassifier::new(&config(&server.uri()), reqwest::Client::new()).unwrap()
}

/// A System One answer choosing `choice`, shaped like Jev's: `confidence`
/// before `probabilities`, and those not in the order the options were asked.
fn chose(choice: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "model": "jev-1.13.0",
        "answers": {"tier": {
            "type": "choice",
            "choice": choice,
            "confidence": 0.82,
            "probabilities": {"simple": 0.85, "complex": 0.15}
        }},
        "usage": {"input_tokens": 312, "output_tokens": 48}
    }))
}

/// A server that answers every System One call with `response`.
async fn server(response: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(response)
        .mount(&server)
        .await;
    server
}

async fn calls(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

/// A resolver for [`gateway_config`] whose classifier is a real Jev backend
/// talking to `server`. `classifier` overrides fields of its config.
fn resolver(server: &MockServer, mut classifier: Value) -> RouteResolver {
    classifier["base_url"] = json!(server.uri());
    let config = gateway_config(classifier);
    let router = ModelRouter::from_config(&config, &Upstream::registry()).unwrap();
    RouteResolver::build(&config, router).unwrap()
}

/// Resolve `auto` once; the reason and the alias that serves the request.
async fn resolve(resolver: &RouteResolver) -> (Reason, String) {
    let decision = resolver.resolve("auto", input).await.unwrap();
    decision.log_classification("req-1");
    let reason = decision.classification.as_ref().unwrap().reason;
    (reason, decision.served_alias().to_string())
}

#[tokio::test]
async fn an_answer_becomes_a_classification() {
    let server = server(chose("simple")).await;

    let classification = jev(&server).classify(&input(0), &tiers()).await;

    assert_eq!(
        classification,
        Ok(Classification {
            tier: "simple".to_string(),
            confidence: Some(0.82),
            // In the tiers' order, whatever order Jev used.
            probabilities: vec![("complex".to_string(), 0.15), ("simple".to_string(), 0.85)],
            model: "jev-1.13.0".to_string(),
            input_tokens: 312,
        })
    );
}

#[tokio::test]
async fn the_request_is_one_choice_question_with_the_configured_key() {
    let server = server(chose("simple")).await;
    // A trailing slash on `base_url` is not doubled.
    let config = config(&format!("{}/", server.uri()));
    let jev = JevClassifier::new(&config, reqwest::Client::new()).unwrap();

    jev.classify(&input(0), &tiers()).await.unwrap();

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.url.path(), "/v1/systemone");
    assert_eq!(request.headers["authorization"], "Bearer ts-secret");
    assert_eq!(request.headers["content-type"], "application/json");
    assert_eq!(
        request.body_json::<Value>().unwrap(),
        json!({
            "state": [
                {"role": "user", "text": "Hi"},
                {"role": "assistant", "text": "Hello, how can I help?"},
                {"role": "user", "text": USER_TEXT},
            ],
            "model": "jev-latest",
            "questions": {"tier": {
                "type": "choice",
                "instructions":
                    "Which option best describes the latest user request in this conversation?",
                "criteria": {
                    "complex": "Multi-step reasoning or code",
                    "simple": "Short factual or chat",
                },
            }},
        })
    );
}

#[tokio::test]
async fn an_option_that_was_not_offered_is_an_unknown_choice() {
    let server = server(chose("medium")).await;

    let classification = jev(&server).classify(&input(0), &tiers()).await;
    assert_eq!(classification, Err(ClassifierError::UnknownChoice));

    let resolver = resolver(&server, json!({}));
    assert_eq!(
        resolve(&resolver).await,
        (Reason::UnknownChoice, "smart".to_string())
    );
}

#[tokio::test]
async fn an_error_status_fails_without_a_retry() {
    let failed =
        |status: u16| ClassifierError::Failed(format!("classifier returned HTTP {status}"));
    let unavailable =
        |status: u16| ClassifierError::Unavailable(format!("classifier returned HTTP {status}"));
    let cases = [
        (401, failed(401)),
        (422, failed(422)),
        (408, unavailable(408)),
        (429, unavailable(429)),
        (529, unavailable(529)),
    ];

    for (status, error) in cases {
        let server = server(ResponseTemplate::new(status)).await;

        let classification = jev(&server).classify(&input(0), &tiers()).await;

        assert_eq!(classification, Err(error), "{status}");
        assert_eq!(calls(&server).await, 1, "{status}");
    }
}

#[tokio::test]
async fn a_malformed_body_fails() {
    let bodies = [
        "not json".to_string(),
        json!({}).to_string(),
        // No answer to the question that was asked.
        json!({"model": "jev-1.13.0", "answers": {}}).to_string(),
        json!({"answers": {"tier": {"type": "choice"}}}).to_string(),
    ];

    for body in bodies {
        let server = server(ResponseTemplate::new(200).set_body_string(body.clone())).await;

        let classification = jev(&server).classify(&input(0), &tiers()).await;

        assert_eq!(
            classification,
            Err(ClassifierError::Failed(
                "classifier response is malformed".to_string()
            )),
            "{body}"
        );
    }
}

#[tokio::test]
async fn only_the_choice_is_required_of_an_answer() {
    let body = json!({"answers": {"tier": {"choice": "complex"}}});
    let server = server(ResponseTemplate::new(200).set_body_json(body)).await;

    let classification = jev(&server).classify(&input(0), &tiers()).await;

    assert_eq!(
        classification,
        Ok(Classification {
            tier: "complex".to_string(),
            confidence: None,
            probabilities: vec![],
            model: String::new(),
            input_tokens: 0,
        })
    );
}

#[tokio::test]
async fn a_server_that_cannot_be_reached_is_unavailable() {
    // A port nothing listens on: bound, then released.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let jev = JevClassifier::new(&config(&base_url), reqwest::Client::new()).unwrap();

    let classification = jev.classify(&input(0), &tiers()).await;

    assert_eq!(
        classification,
        Err(ClassifierError::Unavailable(
            "classifier request failed (connect)".to_string()
        ))
    );
}

#[test]
fn a_key_that_cannot_be_a_header_fails_at_startup() {
    let mut config = config("http://localhost");
    config.api_key = "line\nbreak".to_string();

    let err = JevClassifier::new(&config, reqwest::Client::new())
        .err()
        .unwrap();

    assert_eq!(
        err.to_string(),
        "Classifier 'jev-main': api_key is not a valid HTTP header value"
    );
}

#[test]
fn a_base_url_that_is_not_http_fails_at_startup() {
    for base_url in [
        "api.typesafe.ai",
        "ftp://api.typesafe.ai",
        "https://api typesafe",
    ] {
        let err = JevClassifier::new(&config(base_url), reqwest::Client::new())
            .err()
            .expect(base_url);

        assert_eq!(
            err.to_string(),
            "Classifier 'jev-main': base_url is not a valid http(s) URL",
            "{base_url}"
        );
    }
}

/// Jev's 422 body is a FastAPI error list that echoes the request, `state`
/// included. Neither it nor the request text may reach the logs.
#[tokio::test]
#[traced_test]
async fn a_422_body_never_reaches_the_logs() {
    let echo = json!({"detail": [{
        "type": "string_type", "loc": ["body", "state"], "msg": "echoed-validation-message",
        "input": [{"role": "user", "text": USER_TEXT}]
    }]});
    let response = ResponseTemplate::new(422)
        .insert_header("x-typesafe-request-id", "req_abc123")
        .set_body_json(echo);
    let server = server(response).await;
    let resolver = resolver(&server, json!({}));

    assert_eq!(
        resolve(&resolver).await,
        (Reason::Error, "smart".to_string())
    );

    // What is logged: the status and the id TypeSafe support asks for, at
    // error level, and the resolver's own line.
    assert!(logs_contain("ERROR"));
    assert!(logs_contain(
        "Classifier rejected the request; check its configuration"
    ));
    assert!(logs_contain("status=422"));
    assert!(logs_contain("typesafe_request_id=\"req_abc123\""));
    assert!(logs_contain("Classified request"));
    assert!(logs_contain("classifier returned HTTP 422"));
    // What is not.
    assert!(!logs_contain(USER_TEXT));
    assert!(!logs_contain("echoed-validation-message"));
    assert!(!logs_contain("ts-secret"));
}

#[tokio::test]
async fn an_answer_slower_than_the_timeout_serves_the_fallback() {
    let slow = chose("simple").set_delay(Duration::from_secs(5));
    let server = server(slow).await;
    let resolver = resolver(&server, json!({"timeout_ms": 50}));

    assert_eq!(
        resolve(&resolver).await,
        (Reason::Timeout, "smart".to_string())
    );
    assert_eq!(calls(&server).await, 1);
}

/// Breaker settings that open on the second failure and allow a probe one
/// second later.
fn breaker() -> Value {
    json!({"circuit_breaker": {
        "failure_threshold": 2, "success_threshold": 1, "recovery_timeout_secs": 1
    }})
}

#[tokio::test]
async fn the_breaker_opens_at_the_threshold_and_recovers_through_half_open() {
    let server = server(ResponseTemplate::new(529)).await;
    let resolver = resolver(&server, breaker());
    let fallback = |reason| (reason, "smart".to_string());

    // Closed: every request calls the classifier and is served by the
    // fallback alias.
    assert_eq!(resolve(&resolver).await, fallback(Reason::Error));
    assert_eq!(resolve(&resolver).await, fallback(Reason::Error));
    assert_eq!(calls(&server).await, 2);

    // Open: still the fallback alias, and the classifier is left alone.
    for _ in 0..3 {
        assert_eq!(resolve(&resolver).await, fallback(Reason::BreakerOpen));
    }
    assert_eq!(calls(&server).await, 2);

    // Half-open after the recovery timeout: one probe goes through. It
    // fails, so the breaker opens again.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(resolve(&resolver).await, fallback(Reason::Error));
    assert_eq!(resolve(&resolver).await, fallback(Reason::BreakerOpen));
    assert_eq!(calls(&server).await, 3);

    // The classifier is back: the next probe succeeds and closes the breaker.
    server.reset().await;
    Mock::given(method("POST"))
        .respond_with(chose("simple"))
        .mount(&server)
        .await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    for _ in 0..3 {
        assert_eq!(
            resolve(&resolver).await,
            (Reason::Classified, "fast".to_string())
        );
    }
    assert_eq!(calls(&server).await, 3);
}

/// Timeouts, 429, 5xx and connection errors say the classifier is down. A
/// rejected key or request (401, 422), a malformed body and an option that
/// was not offered do not: the breaker stays closed and the classifier keeps
/// being called.
#[tokio::test]
async fn only_an_unavailable_classifier_trips_the_breaker() {
    let slow = chose("simple").set_delay(Duration::from_secs(5));
    let trips = [
        ResponseTemplate::new(429),
        ResponseTemplate::new(500),
        ResponseTemplate::new(529),
        slow,
    ];
    for response in trips {
        let server = server(response).await;
        let resolver = resolver(
            &server,
            json!({"circuit_breaker": {"failure_threshold": 1}}),
        );

        let (first, _) = resolve(&resolver).await;
        assert!(
            matches!(first, Reason::Error | Reason::Timeout),
            "{first:?}"
        );
        assert_eq!(resolve(&resolver).await.0, Reason::BreakerOpen);
        assert_eq!(calls(&server).await, 1);
    }

    let does_not_trip = [
        (ResponseTemplate::new(401), Reason::Error),
        (ResponseTemplate::new(422), Reason::Error),
        (
            ResponseTemplate::new(200).set_body_string("not json"),
            Reason::Error,
        ),
        (chose("medium"), Reason::UnknownChoice),
    ];
    for (response, reason) in does_not_trip {
        let server = server(response).await;
        let resolver = resolver(
            &server,
            json!({"circuit_breaker": {"failure_threshold": 1}}),
        );

        for _ in 0..3 {
            assert_eq!(resolve(&resolver).await.0, reason);
        }
        assert_eq!(calls(&server).await, 3, "{reason:?}");
    }
}

/// A probe that comes back with a rejected key says nothing about whether
/// the classifier is up: the breaker stays half-open and the next request
/// probes again.
#[tokio::test]
async fn a_probe_that_is_rejected_leaves_the_breaker_half_open() {
    let server = server(ResponseTemplate::new(529)).await;
    let resolver = resolver(
        &server,
        json!({"circuit_breaker": {
            "failure_threshold": 1, "success_threshold": 1, "recovery_timeout_secs": 0
        }}),
    );
    assert_eq!(resolve(&resolver).await.0, Reason::Error);

    server.reset().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    for _ in 0..3 {
        assert_eq!(resolve(&resolver).await.0, Reason::Error);
    }
    assert_eq!(calls(&server).await, 3);

    server.reset().await;
    Mock::given(method("POST"))
        .respond_with(chose("simple"))
        .mount(&server)
        .await;
    assert_eq!(resolve(&resolver).await.0, Reason::Classified);
}

/// One breaker per classifier: failures seen through one alias keep every
/// alias using that classifier from calling it.
#[tokio::test]
async fn aliases_sharing_a_classifier_share_its_breaker() {
    let server = server(ResponseTemplate::new(529)).await;
    let mut config = gateway_config(json!({
        "base_url": server.uri(),
        "circuit_breaker": {"failure_threshold": 1, "recovery_timeout_secs": 30},
    }));
    let mut second = config.models.last().unwrap().clone();
    second.alias = "auto-2".to_string();
    config.models.push(second);
    let router = ModelRouter::from_config(&config, &Upstream::registry()).unwrap();
    let resolver = RouteResolver::build(&config, router).unwrap();

    assert_eq!(resolve(&resolver).await.0, Reason::Error);

    let decision = resolver.resolve("auto-2", input).await.unwrap();
    assert_eq!(decision.served_alias(), "smart");
    assert_eq!(decision.classification.unwrap().reason, Reason::BreakerOpen);
    assert_eq!(calls(&server).await, 1);
}

/// With the breaker open a request is not even read for its text.
#[tokio::test]
async fn an_open_breaker_skips_input_extraction() {
    let server = server(ResponseTemplate::new(529)).await;
    let resolver = resolver(
        &server,
        json!({"circuit_breaker": {"failure_threshold": 1, "recovery_timeout_secs": 30}}),
    );
    assert_eq!(resolve(&resolver).await.0, Reason::Error);

    let decision = resolver
        .resolve("auto", |_| panic!("an open breaker must not extract input"))
        .await
        .unwrap();

    assert_eq!(decision.served_alias(), "smart");
    let record = decision.classification.unwrap();
    assert_eq!(record.reason, Reason::BreakerOpen);
    assert_eq!(record.error, Some(ClassifierError::BreakerOpen));
}

/// The real API, with the key in `TYPESAFE_API_KEY`:
/// `cargo test -p ferrox live_jev -- --ignored`. Answers are not
/// deterministic, so only their shape is checked.
#[tokio::test]
#[ignore = "calls the live TypeSafe API; needs TYPESAFE_API_KEY"]
async fn live_jev_answers_with_an_offered_tier() {
    let api_key = std::env::var("TYPESAFE_API_KEY").expect("TYPESAFE_API_KEY is not set");
    let config: ClassifierConfig =
        serde_json::from_value(json!({"id": "live", "type": "jev", "api_key": api_key})).unwrap();
    let jev = JevClassifier::new(&config, reqwest::Client::new()).unwrap();
    let tiers = tiers();

    let answer = tokio::time::timeout(Duration::from_secs(30), jev.classify(&input(0), &tiers))
        .await
        .expect("no answer within 30s")
        .expect("classification failed");

    assert!(tiers.iter().any(|t| t.name == answer.tier), "{answer:?}");
    let confidence = answer.confidence.expect("a choice has a confidence");
    assert!((0.0..=1.0).contains(&confidence), "{answer:?}");
    assert_eq!(answer.probabilities.len(), tiers.len(), "{answer:?}");
    let total: f64 = answer.probabilities.iter().map(|(_, p)| p).sum();
    assert!((total - 1.0).abs() < 0.05, "{answer:?}");
    assert!(answer.model.starts_with("jev-"), "{answer:?}");
    assert!(answer.input_tokens > 0, "{answer:?}");
}
