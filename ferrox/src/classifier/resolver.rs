//! [`RouteResolver`]: the one place an inbound handler turns a model alias
//! into the pool that serves it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _};
use axum::http::HeaderMap;

use super::{
    Classification, Classifier, ClassifierError, ClassifierInput, ClassifierRegistry, Tier,
};
use crate::config::Config;
use crate::error::ProxyError;
use crate::lb::circuit_breaker::CircuitBreaker;
use crate::lb::RoutePool;
use crate::router::ModelRouter;

/// Request header a client sets to [`SKIP_CLASSIFIER`] to have a classified
/// alias served by its `fallback_alias` without the classifier being called.
const CLASSIFIER_HEADER: &str = "x-ferrox-classifier";
const SKIP_CLASSIFIER: &str = "skip";

/// Whether the request opts out of classification. Any other value of the
/// header is ignored.
pub fn skip_requested(headers: &HeaderMap) -> bool {
    headers
        .get(CLASSIFIER_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case(SKIP_CLASSIFIER))
}

/// How a classified request came to be served by the alias that served it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The classifier chose a tier; its alias serves the request.
    Classified,
    /// The answer's confidence was below the alias's `confidence_threshold`.
    LowConfidence,
    /// The classifier did not answer within its `timeout_ms`.
    Timeout,
    /// The classifier failed, or the request had nothing to classify.
    Error,
    /// The classifier answered with a tier the alias does not list.
    UnknownChoice,
    /// The classifier's circuit breaker is open, so it was not called.
    BreakerOpen,
    /// The alias is in shadow mode: the classifier chose a tier that would
    /// have served the request, and `fallback_alias` served it instead.
    Shadow,
    /// The client asked to skip the classifier, so it was not called.
    OptOut,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Classified => "classified",
            Self::LowConfidence => "low_confidence",
            Self::Timeout => "timeout",
            Self::Error => "error",
            Self::UnknownChoice => "unknown_choice",
            Self::BreakerOpen => "breaker_open",
            Self::Shadow => "shadow",
            Self::OptOut => "opt_out",
        }
    }
}

/// What the classifier did for one request to a classified alias.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassificationRecord {
    pub reason: Reason,
    /// The classifier's answer, when it gave one — also when the answer was
    /// not followed (`low_confidence`, `unknown_choice`, `shadow`).
    pub classification: Option<Classification>,
    /// Why there is no answer (`timeout`, `error`, `breaker_open`, and an
    /// `unknown_choice` the backend itself refused). An `opt_out` has neither
    /// an answer nor an error: the classifier was never asked.
    pub error: Option<ClassifierError>,
    /// Time spent on classification, input extraction included.
    pub latency: Duration,
}

/// The outcome of resolving a requested alias.
pub struct RouteDecision<'a> {
    /// The alias the client asked for. Authorization is checked against it.
    pub requested_alias: &'a str,
    /// The pool that serves the request. Its alias is the served alias.
    pub pool: Arc<RoutePool>,
    /// Set when the requested alias is a classified one.
    pub classification: Option<ClassificationRecord>,
}

impl RouteDecision<'_> {
    /// The statically routed alias that serves the request: the requested
    /// alias itself, or the tier or fallback alias a classified one resolved
    /// to. Logs, metrics and usage are recorded under it.
    pub fn served_alias(&self) -> &str {
        &self.pool.alias
    }

    /// Log the classifier's part in this decision. Silent for a statically
    /// routed alias.
    pub fn log_classification(&self, request_id: &str) {
        let Some(record) = &self.classification else {
            return;
        };
        let answer = record.classification.as_ref();
        tracing::info!(
            request_id = %request_id,
            requested_alias = %self.requested_alias,
            served_alias = %self.served_alias(),
            reason = record.reason.as_str(),
            tier = answer.map(|c| c.tier.as_str()),
            confidence = answer.and_then(|c| c.confidence),
            probabilities = answer.map(|c| tracing::field::debug(&c.probabilities)),
            classifier_model = answer.map(|c| c.model.as_str()),
            classifier_input_tokens = answer.map(|c| c.input_tokens),
            classifier_latency_ms = record.latency.as_millis() as u64,
            error = record.error.as_ref().map(tracing::field::display),
            "Classified request"
        );
    }
}

/// A classified alias, with everything a request needs resolved at startup.
struct ClassifiedRoute {
    classifier: Arc<dyn Classifier>,
    /// The classifier's breaker, shared by every alias that uses it.
    breaker: Arc<CircuitBreaker>,
    tiers: Vec<Tier>,
    /// The pool serving each tier; parallel to `tiers`.
    tier_pools: Vec<Arc<RoutePool>>,
    fallback: Arc<RoutePool>,
    /// Shadow mode: an answer that would have been followed is recorded, and
    /// `fallback` serves the request all the same.
    shadow: bool,
    confidence_threshold: f64,
    timeout: Duration,
    max_input_chars: usize,
}

impl ClassifiedRoute {
    /// The pool to serve from, and the record of why.
    fn decide(
        &self,
        outcome: Result<Classification, ClassifierError>,
    ) -> (
        Arc<RoutePool>,
        Reason,
        Option<Classification>,
        Option<ClassifierError>,
    ) {
        let fallback = || self.fallback.clone();
        match outcome {
            Err(ClassifierError::Timeout) => (
                fallback(),
                Reason::Timeout,
                None,
                Some(ClassifierError::Timeout),
            ),
            Err(e) => {
                let reason = match e {
                    ClassifierError::BreakerOpen => Reason::BreakerOpen,
                    ClassifierError::UnknownChoice => Reason::UnknownChoice,
                    _ => Reason::Error,
                };
                (fallback(), reason, None, Some(e))
            }
            Ok(answer) => {
                let Some(tier) = self.tiers.iter().position(|t| t.name == answer.tier) else {
                    return (fallback(), Reason::UnknownChoice, Some(answer), None);
                };
                // A confidence that is not a number is not a confident answer.
                let unsure = answer
                    .confidence
                    .is_some_and(|c| c.is_nan() || c < self.confidence_threshold);
                if unsure {
                    return (fallback(), Reason::LowConfidence, Some(answer), None);
                }
                if self.shadow {
                    return (fallback(), Reason::Shadow, Some(answer), None);
                }
                (
                    self.tier_pools[tier].clone(),
                    Reason::Classified,
                    Some(answer),
                    None,
                )
            }
        }
    }
}

impl ClassifiedRoute {
    /// The decision for a request to `alias` whose classification, begun at
    /// `start`, ended in `outcome`.
    fn decision<'a>(
        &self,
        alias: &'a str,
        outcome: Result<Classification, ClassifierError>,
        start: Instant,
    ) -> RouteDecision<'a> {
        let (pool, reason, classification, error) = self.decide(outcome);
        RouteDecision {
            requested_alias: alias,
            pool,
            classification: Some(ClassificationRecord {
                reason,
                classification,
                error,
                latency: start.elapsed(),
            }),
        }
    }
}

/// Resolves a requested model alias to the pool that serves it.
///
/// A statically routed alias resolves exactly as [`ModelRouter::resolve`]
/// does. A classified alias (`models[].classifier`) is resolved per request
/// by its classifier, to one of its tiers' aliases or to its
/// `fallback_alias`; from there the request is an ordinary one to that
/// alias, with the same strategy, retries, failover and circuit breakers.
pub struct RouteResolver {
    router: ModelRouter,
    classified: HashMap<String, ClassifiedRoute>,
}

impl RouteResolver {
    /// The resolver for `config`, with the classifier backends it names.
    pub fn build(config: &Config, router: ModelRouter) -> Result<Self, anyhow::Error> {
        Self::from_config(config, router, &super::build_registry(&config.classifiers)?)
    }

    /// `classifiers` holds the backend of every entry in `config.classifiers`.
    /// Every tier and fallback pool is looked up here, once, so no request
    /// can fail on one.
    pub fn from_config(
        config: &Config,
        router: ModelRouter,
        classifiers: &ClassifierRegistry,
    ) -> Result<Self, anyhow::Error> {
        // One breaker per classifier, however many aliases use it. A pool's
        // breaker is labelled with a provider name and a model alias; the
        // empty alias keeps a classifier's apart from every one of those.
        let breakers: HashMap<&str, Arc<CircuitBreaker>> = config
            .classifiers
            .iter()
            .map(|c| {
                let settings = c
                    .circuit_breaker
                    .clone()
                    .unwrap_or_else(|| config.defaults.circuit_breaker.clone());
                let breaker = CircuitBreaker::new(settings, format!("classifier:{}", c.id), "");
                (c.id.as_str(), Arc::new(breaker))
            })
            .collect();

        let mut classified = HashMap::new();
        for model in &config.models {
            let Some(alias) = &model.classifier else {
                continue;
            };
            let unknown = || {
                anyhow!(
                    "Model '{}' uses unknown classifier '{}'",
                    model.alias,
                    alias.classifier
                )
            };
            let settings = config
                .classifiers
                .iter()
                .find(|c| c.id == alias.classifier)
                .ok_or_else(unknown)?;
            let classifier = classifiers
                .get(&alias.classifier)
                .ok_or_else(unknown)?
                .clone();
            let pool = |target: &str| {
                router
                    .resolve(target)
                    .with_context(|| format!("Model '{}' cannot be classified", model.alias))
            };

            let mut tiers = Vec::with_capacity(alias.tiers.len());
            let mut tier_pools = Vec::with_capacity(alias.tiers.len());
            for (name, tier) in &alias.tiers {
                tier_pools.push(pool(&tier.alias)?);
                tiers.push(Tier {
                    name: name.clone(),
                    when: tier.when.clone(),
                });
            }
            classified.insert(
                model.alias.clone(),
                ClassifiedRoute {
                    classifier,
                    breaker: breakers[settings.id.as_str()].clone(),
                    tiers,
                    tier_pools,
                    fallback: pool(&alias.fallback_alias)?,
                    shadow: alias.shadow,
                    confidence_threshold: alias.confidence_threshold,
                    timeout: Duration::from_millis(settings.timeout_ms),
                    max_input_chars: settings.max_input_chars,
                },
            );
        }
        Ok(Self { router, classified })
    }

    /// Resolve `alias`. `input` builds the classifier's view of the request,
    /// given the cap on its size; it is called only for a classified alias,
    /// so a statically routed one costs the same single map lookup as
    /// [`ModelRouter::resolve`].
    ///
    /// The classifier never fails a request: the only error is the ordinary
    /// "not configured" one for an alias that does not exist. While its
    /// circuit breaker is open it is not called at all, so a classifier that
    /// keeps timing out stops adding its `timeout_ms` to every request.
    ///
    /// `skip_classifier` is the client's opt-out ([`skip_requested`]): a
    /// classified alias is then served by its `fallback_alias` and the
    /// classifier is left alone. It is never looked at for a statically
    /// routed alias.
    pub async fn resolve<'a>(
        &self,
        alias: &'a str,
        skip_classifier: bool,
        input: impl FnOnce(usize) -> ClassifierInput,
    ) -> Result<RouteDecision<'a>, ProxyError> {
        let not_configured = match self.router.resolve(alias) {
            Ok(pool) => {
                return Ok(RouteDecision {
                    requested_alias: alias,
                    pool,
                    classification: None,
                })
            }
            Err(e) => e,
        };
        let Some(route) = self.classified.get(alias) else {
            return Err(not_configured);
        };

        let start = Instant::now();
        // Before the breaker and the input: an opted-out request costs
        // neither, and says nothing about the classifier's health.
        if skip_classifier {
            return Ok(RouteDecision {
                requested_alias: alias,
                pool: route.fallback.clone(),
                classification: Some(ClassificationRecord {
                    reason: Reason::OptOut,
                    classification: None,
                    error: None,
                    latency: start.elapsed(),
                }),
            });
        }
        // Asked before the input is built: an open breaker costs a request
        // no extraction either. Reads only, so nothing is claimed for a
        // request that then has nothing to classify.
        let input = if route.breaker.can_serve() {
            input(route.max_input_chars)
        } else {
            return Ok(route.decision(alias, Err(ClassifierError::BreakerOpen), start));
        };
        let outcome = if input.turns.is_empty() {
            Err(ClassifierError::NoInput)
        } else if let Some(permit) = route.breaker.try_acquire() {
            let answer = route.classifier.classify(&input, &route.tiers);
            let outcome = match tokio::time::timeout(route.timeout, answer).await {
                Ok(outcome) => outcome,
                Err(_) => Err(ClassifierError::Timeout),
            };
            match &outcome {
                // An answer nobody can use is still a classifier that works.
                Ok(_) | Err(ClassifierError::UnknownChoice) => permit.success(),
                Err(ClassifierError::Timeout | ClassifierError::Unavailable(_)) => permit.failure(),
                // Says nothing about the classifier's health: leave the
                // breaker as it is.
                Err(_) => drop(permit),
            }
            outcome
        } else {
            Err(ClassifierError::BreakerOpen)
        };
        Ok(route.decision(alias, outcome, start))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::classifier::input::{Role, Turn};
    use crate::classifier::test_support::{
        answer, gateway_config, registry_with, shadowed, Answer, FakeClassifier, Upstream,
    };

    fn resolver(classifier: &Arc<FakeClassifier>) -> RouteResolver {
        resolver_for(&gateway_config(json!({})), classifier)
    }

    fn resolver_for(config: &Config, classifier: &Arc<FakeClassifier>) -> RouteResolver {
        let providers = Upstream::registry();
        let router = ModelRouter::from_config(config, &providers).unwrap();
        RouteResolver::from_config(config, router, &registry_with(classifier.clone())).unwrap()
    }

    fn input(_max_input_chars: usize) -> ClassifierInput {
        ClassifierInput {
            turns: vec![Turn {
                role: Role::User,
                text: "hi".to_string(),
            }],
        }
    }

    /// One case per reason, live and in shadow mode. The alias has tiers
    /// `simple` → `fast` and `complex` → `smart`, falls back to `smart`, and
    /// requires a confidence of 0.6, so only a followed `simple` answer is
    /// served by `fast`. In shadow mode nothing is: an answer that would
    /// have been followed is a `shadow`, every other outcome is unchanged.
    #[tokio::test]
    async fn every_outcome_has_a_reason_and_a_served_alias() {
        let failed = ClassifierError::Failed("upstream said 529".to_string());
        // (reply, reason and served alias live, reason in shadow mode, error)
        let cases = [
            (
                Answer::Tier("simple", Some(0.9)),
                (Reason::Classified, "fast"),
                Reason::Shadow,
                None,
            ),
            // A backend without calibrated confidence is taken at its word.
            (
                Answer::Tier("simple", None),
                (Reason::Classified, "fast"),
                Reason::Shadow,
                None,
            ),
            // Exactly the threshold is confident enough.
            (
                Answer::Tier("simple", Some(0.6)),
                (Reason::Classified, "fast"),
                Reason::Shadow,
                None,
            ),
            // A tier whose alias is the fallback's is still a followed answer.
            (
                Answer::Tier("complex", Some(0.9)),
                (Reason::Classified, "smart"),
                Reason::Shadow,
                None,
            ),
            (
                Answer::Tier("simple", Some(0.59)),
                (Reason::LowConfidence, "smart"),
                Reason::LowConfidence,
                None,
            ),
            (
                Answer::Tier("simple", Some(f64::NAN)),
                (Reason::LowConfidence, "smart"),
                Reason::LowConfidence,
                None,
            ),
            (
                Answer::Tier("medium", Some(0.9)),
                (Reason::UnknownChoice, "smart"),
                Reason::UnknownChoice,
                None,
            ),
            (
                Answer::Fail(failed.clone()),
                (Reason::Error, "smart"),
                Reason::Error,
                Some(failed),
            ),
            (
                Answer::Fail(ClassifierError::Timeout),
                (Reason::Timeout, "smart"),
                Reason::Timeout,
                Some(ClassifierError::Timeout),
            ),
            // Slower than `timeout_ms`: cut off by the resolver.
            (
                Answer::Hang,
                (Reason::Timeout, "smart"),
                Reason::Timeout,
                Some(ClassifierError::Timeout),
            ),
        ];

        for (reply, live, shadow_reason, error) in cases {
            for (shadow, (reason, served)) in [(false, live), (true, (shadow_reason, "smart"))] {
                let classifier = FakeClassifier::new(reply.clone());
                let mut config = gateway_config(json!({}));
                if shadow {
                    config = shadowed(config);
                }
                let resolver = resolver_for(&config, &classifier);

                let decision = resolver.resolve("auto", false, input).await.unwrap();
                let case = format!("{reply:?} shadow={shadow}");
                assert_eq!(decision.requested_alias, "auto", "{case}");
                assert_eq!(decision.served_alias(), served, "{case}");
                assert_eq!(classifier.calls(), 1, "{case}");

                let record = decision.classification.expect(&case);
                assert_eq!(record.reason, reason, "{case}");
                assert_eq!(record.error, error, "{case}");
                match reply {
                    // The answer is recorded whether or not it was followed.
                    // NaN never equals itself, so compare the tier.
                    Answer::Tier(tier, _) => {
                        assert_eq!(record.classification.expect(&case).tier, tier, "{case}");
                    }
                    _ => assert_eq!(record.classification, None, "{case}"),
                }
            }
        }
    }

    /// Shadow mode keeps the whole answer the classifier would have been
    /// followed on, not just its tier.
    #[tokio::test]
    async fn shadow_records_the_would_be_tier_with_its_confidence() {
        let classifier = FakeClassifier::new(Answer::Tier("simple", Some(0.9)));
        let resolver = resolver_for(&shadowed(gateway_config(json!({}))), &classifier);

        let decision = resolver.resolve("auto", false, input).await.unwrap();

        assert_eq!(decision.served_alias(), "smart");
        assert_eq!(classifier.calls(), 1);
        let record = decision.classification.unwrap();
        assert_eq!(record.reason, Reason::Shadow);
        assert_eq!(record.classification, Some(answer("simple", Some(0.9))));
        assert_eq!(record.error, None);
    }

    /// Opting out serves the fallback without reading the request or asking
    /// the classifier, in shadow mode too.
    #[tokio::test]
    async fn an_opt_out_never_builds_the_input_or_calls_the_classifier() {
        for shadow in [false, true] {
            let classifier = FakeClassifier::new(Answer::Tier("simple", Some(0.9)));
            let mut config = gateway_config(json!({}));
            if shadow {
                config = shadowed(config);
            }
            let resolver = resolver_for(&config, &classifier);

            let decision = resolver
                .resolve("auto", true, |_| {
                    panic!("an opt-out must not extract input")
                })
                .await
                .unwrap();

            assert_eq!(decision.requested_alias, "auto", "shadow={shadow}");
            assert_eq!(decision.served_alias(), "smart", "shadow={shadow}");
            assert_eq!(classifier.calls(), 0, "shadow={shadow}");
            let record = decision.classification.unwrap();
            assert_eq!(record.reason, Reason::OptOut, "shadow={shadow}");
            assert_eq!(record.classification, None, "shadow={shadow}");
            assert_eq!(record.error, None, "shadow={shadow}");
        }
    }

    /// The opt-out is decided before the breaker is asked: it is reported as
    /// an opt-out while the breaker is open, and leaves the breaker as it is.
    #[tokio::test]
    async fn an_opt_out_is_reported_as_one_while_the_breaker_is_open() {
        let classifier = FakeClassifier::new(Answer::Fail(ClassifierError::Timeout));
        let config = gateway_config(json!({
            "circuit_breaker": {"failure_threshold": 1, "recovery_timeout_secs": 30},
        }));
        let resolver = resolver_for(&config, &classifier);
        let reason = |decision: RouteDecision<'_>| decision.classification.unwrap().reason;

        let tripped = resolver.resolve("auto", false, input).await.unwrap();
        assert_eq!(reason(tripped), Reason::Timeout);

        let opted_out = resolver.resolve("auto", true, input).await.unwrap();
        assert_eq!(opted_out.served_alias(), "smart");
        assert_eq!(reason(opted_out), Reason::OptOut);

        let next = resolver.resolve("auto", false, input).await.unwrap();
        assert_eq!(reason(next), Reason::BreakerOpen);
        assert_eq!(classifier.calls(), 1);
    }

    #[test]
    fn only_the_skip_value_of_the_header_opts_out() {
        let cases = [
            (Some("skip"), true),
            (Some("SKIP"), true),
            (Some("Skip"), true),
            (Some(" skip "), true),
            (Some(""), false),
            (Some("skipped"), false),
            (Some("true"), false),
            (Some("fast"), false),
            (None, false),
        ];
        for (value, skips) in cases {
            let mut headers = HeaderMap::new();
            if let Some(value) = value {
                headers.insert(CLASSIFIER_HEADER, value.parse().unwrap());
            }
            assert_eq!(skip_requested(&headers), skips, "{value:?}");
        }

        // Not text at all: ignored like any other value.
        let mut headers = HeaderMap::new();
        let opaque = axum::http::HeaderValue::from_bytes(b"sk\xffip").unwrap();
        headers.insert(CLASSIFIER_HEADER, opaque);
        assert!(!skip_requested(&headers));
    }

    #[tokio::test]
    async fn a_request_with_nothing_to_classify_serves_the_fallback_without_a_call() {
        let classifier = FakeClassifier::new(Answer::Tier("simple", Some(0.9)));
        let resolver = resolver(&classifier);

        let decision = resolver
            .resolve("auto", false, |_| ClassifierInput::default())
            .await
            .unwrap();

        assert_eq!(decision.served_alias(), "smart");
        assert_eq!(classifier.calls(), 0);
        let record = decision.classification.unwrap();
        assert_eq!(record.reason, Reason::Error);
        assert_eq!(record.error, Some(ClassifierError::NoInput));
    }

    #[tokio::test]
    async fn the_classifier_sees_the_capped_input_and_the_alias_tiers() {
        let classifier = FakeClassifier::new(Answer::Tier("complex", Some(1.0)));
        let config = gateway_config(json!({"max_input_chars": 42}));
        let resolver = resolver_for(&config, &classifier);

        let decision = resolver
            .resolve("auto", false, |max_input_chars| {
                assert_eq!(max_input_chars, 42);
                input(max_input_chars)
            })
            .await
            .unwrap();

        assert_eq!(decision.served_alias(), "smart");
        let record = decision.classification.unwrap();
        assert_eq!(record.reason, Reason::Classified);
        assert_eq!(record.classification, Some(answer("complex", Some(1.0))));
        let (seen, tiers) = classifier.seen().unwrap();
        assert_eq!(seen, input(0));
        assert_eq!(tiers, ["complex", "simple"]);
    }

    #[tokio::test]
    async fn a_static_alias_never_builds_the_input_or_calls_the_classifier() {
        let classifier = FakeClassifier::new(Answer::Tier("simple", Some(0.9)));
        let resolver = resolver(&classifier);

        // The opt-out is not looked at for a static alias.
        for skip_classifier in [false, true] {
            let decision = resolver
                .resolve("fast", skip_classifier, |_| {
                    panic!("a static alias must not extract input")
                })
                .await
                .unwrap();

            assert_eq!(decision.requested_alias, "fast");
            assert_eq!(decision.served_alias(), "fast");
            assert!(decision.classification.is_none());
        }
        assert_eq!(classifier.calls(), 0);
    }

    #[tokio::test]
    async fn an_unknown_alias_is_the_ordinary_not_configured_error() {
        let classifier = FakeClassifier::new(Answer::Tier("simple", Some(0.9)));
        let resolver = resolver(&classifier);

        let result = resolver
            .resolve("nope", false, |_| {
                panic!("an unknown alias must not extract input")
            })
            .await;

        match result {
            Err(ProxyError::ModelNotFound(msg)) => {
                assert_eq!(msg, "Model alias 'nope' is not configured");
            }
            Err(e) => panic!("unexpected error: {e}"),
            Ok(_) => panic!("an unknown alias must not resolve"),
        }
        assert_eq!(classifier.calls(), 0);
    }

    #[test]
    fn a_classified_alias_without_its_backend_or_pools_fails_at_startup() {
        let config = gateway_config(json!({}));
        let router = || ModelRouter::from_config(&config, &Upstream::registry()).unwrap();

        let err = RouteResolver::from_config(&config, router(), &ClassifierRegistry::new())
            .err()
            .unwrap();
        assert_eq!(
            err.to_string(),
            "Model 'auto' uses unknown classifier 'fake'"
        );

        // Bypassing `validate()`: a fallback that has no pool.
        let mut broken = config.clone();
        let auto = broken
            .models
            .iter_mut()
            .find(|m| m.alias == "auto")
            .unwrap();
        auto.classifier.as_mut().unwrap().fallback_alias = "missing".to_string();
        let classifier = FakeClassifier::new(Answer::Hang);
        let err = RouteResolver::from_config(&broken, router(), &registry_with(classifier))
            .err()
            .unwrap();
        assert_eq!(
            format!("{err:#}"),
            "Model 'auto' cannot be classified: Model not found: Model alias 'missing' is not configured"
        );
    }
}
