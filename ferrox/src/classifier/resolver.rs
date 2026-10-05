//! [`RouteResolver`]: the one place an inbound handler turns a model alias
//! into the pool that serves it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _};

use super::{
    Classification, Classifier, ClassifierError, ClassifierInput, ClassifierRegistry, Tier,
};
use crate::config::Config;
use crate::error::ProxyError;
use crate::lb::circuit_breaker::CircuitBreaker;
use crate::lb::RoutePool;
use crate::router::ModelRouter;

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
        }
    }
}

/// What the classifier did for one request to a classified alias.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassificationRecord {
    pub reason: Reason,
    /// The classifier's answer, when it gave one — also when the answer was
    /// not followed (`low_confidence`, `unknown_choice`).
    pub classification: Option<Classification>,
    /// Why there is no answer (`timeout`, `error`, `breaker_open`, and an
    /// `unknown_choice` the backend itself refused).
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
    pub async fn resolve<'a>(
        &self,
        alias: &'a str,
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
        let input = input(route.max_input_chars);
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
        let (pool, reason, classification, error) = route.decide(outcome);

        Ok(RouteDecision {
            requested_alias: alias,
            pool,
            classification: Some(ClassificationRecord {
                reason,
                classification,
                error,
                latency: start.elapsed(),
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::classifier::input::{Role, Turn};
    use crate::classifier::test_support::{
        answer, gateway_config, registry_with, Answer, FakeClassifier, Upstream,
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

    /// One case per reason. The alias has tiers `simple` → `fast` and
    /// `complex` → `smart`, falls back to `smart`, and requires a confidence
    /// of 0.6, so only a followed `simple` answer is served by `fast`.
    #[tokio::test]
    async fn every_outcome_has_a_reason_and_a_served_alias() {
        let failed = ClassifierError::Failed("upstream said 529".to_string());
        let cases = [
            (
                Answer::Tier("simple", Some(0.9)),
                Reason::Classified,
                "fast",
                None,
            ),
            // A backend without calibrated confidence is taken at its word.
            (
                Answer::Tier("simple", None),
                Reason::Classified,
                "fast",
                None,
            ),
            // Exactly the threshold is confident enough.
            (
                Answer::Tier("simple", Some(0.6)),
                Reason::Classified,
                "fast",
                None,
            ),
            (
                Answer::Tier("simple", Some(0.59)),
                Reason::LowConfidence,
                "smart",
                None,
            ),
            (
                Answer::Tier("simple", Some(f64::NAN)),
                Reason::LowConfidence,
                "smart",
                None,
            ),
            (
                Answer::Tier("medium", Some(0.9)),
                Reason::UnknownChoice,
                "smart",
                None,
            ),
            (
                Answer::Fail(failed.clone()),
                Reason::Error,
                "smart",
                Some(failed),
            ),
            (
                Answer::Fail(ClassifierError::Timeout),
                Reason::Timeout,
                "smart",
                Some(ClassifierError::Timeout),
            ),
            // Slower than `timeout_ms`: cut off by the resolver.
            (
                Answer::Hang,
                Reason::Timeout,
                "smart",
                Some(ClassifierError::Timeout),
            ),
        ];

        for (reply, reason, served, error) in cases {
            let classifier = FakeClassifier::new(reply.clone());
            let resolver = resolver(&classifier);

            let decision = resolver.resolve("auto", input).await.unwrap();
            let case = format!("{reply:?}");
            assert_eq!(decision.requested_alias, "auto", "{case}");
            assert_eq!(decision.served_alias(), served, "{case}");
            assert_eq!(classifier.calls(), 1, "{case}");

            let record = decision.classification.expect(&case);
            assert_eq!(record.reason, reason, "{case}");
            assert_eq!(record.error, error, "{case}");
            match reply {
                // The answer is recorded whether or not it was followed. NaN
                // never equals itself, so compare the tier.
                Answer::Tier(tier, _) => {
                    assert_eq!(record.classification.expect(&case).tier, tier, "{case}");
                }
                _ => assert_eq!(record.classification, None, "{case}"),
            }
        }
    }

    #[tokio::test]
    async fn a_request_with_nothing_to_classify_serves_the_fallback_without_a_call() {
        let classifier = FakeClassifier::new(Answer::Tier("simple", Some(0.9)));
        let resolver = resolver(&classifier);

        let decision = resolver
            .resolve("auto", |_| ClassifierInput::default())
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
            .resolve("auto", |max_input_chars| {
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

        let decision = resolver
            .resolve("fast", |_| panic!("a static alias must not extract input"))
            .await
            .unwrap();

        assert_eq!(decision.requested_alias, "fast");
        assert_eq!(decision.served_alias(), "fast");
        assert!(decision.classification.is_none());
        assert_eq!(classifier.calls(), 0);
    }

    #[tokio::test]
    async fn an_unknown_alias_is_the_ordinary_not_configured_error() {
        let classifier = FakeClassifier::new(Answer::Tier("simple", Some(0.9)));
        let resolver = resolver(&classifier);

        let result = resolver
            .resolve("nope", |_| {
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
