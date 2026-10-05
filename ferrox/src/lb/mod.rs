pub mod circuit_breaker;
pub mod strategy;

use std::sync::Arc;

use crate::config::{DefaultsConfig, ProviderConfig, RoutingConfig, RoutingStrategy};
use crate::providers::{ProviderAdapter, ProviderRegistry};
use crate::telemetry::metrics::ROUTING_TARGET_SELECTED;

use circuit_breaker::{CircuitBreaker, ProbePermit};
use strategy::LbStrategy;

// ── RouteTarget ───────────────────────────────────────────────────────────────

pub struct RouteTarget {
    pub provider: Arc<dyn ProviderAdapter>,
    pub model_id: String,
    pub circuit_breaker: Arc<CircuitBreaker>,
}

impl RouteTarget {
    /// Whether the circuit breaker would let a request through. Reads only;
    /// see [`CircuitBreaker::can_serve`].
    pub fn can_serve(&self) -> bool {
        self.circuit_breaker.can_serve()
    }

    /// Admit a request that is about to be attempted on this target; see
    /// [`CircuitBreaker::try_acquire`].
    pub fn try_acquire(&self) -> Option<ProbePermit<'_>> {
        self.circuit_breaker.try_acquire()
    }
}

// ── RoutePool ─────────────────────────────────────────────────────────────────

pub struct RoutePool {
    pub alias: String,
    strategy: LbStrategy,
    strategy_name: &'static str,
    pub targets: Vec<RouteTarget>,
    pub fallbacks: Vec<RouteTarget>,
}

impl RoutePool {
    pub fn from_config(
        alias: &str,
        routing: &RoutingConfig,
        providers: &ProviderRegistry,
        provider_configs: &[ProviderConfig],
        defaults: &DefaultsConfig,
    ) -> Result<Self, anyhow::Error> {
        let (strategy, strategy_name) = match routing.strategy {
            RoutingStrategy::RoundRobin => (LbStrategy::round_robin(), "round_robin"),
            RoutingStrategy::Failover => (LbStrategy::failover(), "failover"),
            RoutingStrategy::Random => (LbStrategy::random(), "random"),
            RoutingStrategy::Weighted => {
                let weights: Vec<u32> = routing
                    .targets
                    .iter()
                    .map(|t| t.weight.unwrap_or(1))
                    .collect();
                (LbStrategy::weighted(&weights), "weighted")
            }
        };

        let targets = routing
            .targets
            .iter()
            .map(|t| build_target(t, alias, providers, provider_configs, defaults))
            .collect::<Result<Vec<_>, _>>()?;

        let fallbacks = routing
            .fallback
            .iter()
            .map(|t| build_target(t, alias, providers, provider_configs, defaults))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(RoutePool {
            alias: alias.to_string(),
            strategy,
            strategy_name,
            targets,
            fallbacks,
        })
    }

    /// Select the best available primary target among those `eligible` for
    /// this request, and admit the request on its circuit breaker.
    ///
    /// Candidates are filtered with the read-only [`RouteTarget::can_serve`];
    /// only the target the strategy picks is acquired, so a half-open primary
    /// that is not chosen keeps its probe slot free. When another request
    /// takes the pick's probe slot in between, the strategy picks again among
    /// the rest. The returned permit must be settled with the attempt's
    /// outcome. Also records the `routing_target_selected` metric.
    pub fn select_target(
        &self,
        eligible: impl Fn(&RouteTarget) -> bool,
    ) -> Option<(&RouteTarget, ProbePermit<'_>)> {
        let mut available: Vec<bool> = self
            .targets
            .iter()
            .map(|t| eligible(t) && t.can_serve())
            .collect();

        // Each lost race rules one target out, so this ends within
        // `targets.len()` rounds.
        loop {
            let idx = self.strategy.select(&available)?;
            let target = &self.targets[idx];
            let Some(permit) = target.try_acquire() else {
                available[idx] = false;
                continue;
            };

            ROUTING_TARGET_SELECTED
                .with_label_values(&[
                    self.alias.as_str(),
                    target.provider.name(),
                    self.strategy_name,
                ])
                .inc();

            return Some((target, permit));
        }
    }
}

fn build_target(
    target_cfg: &crate::config::TargetConfig,
    model_alias: &str,
    providers: &ProviderRegistry,
    provider_configs: &[ProviderConfig],
    defaults: &DefaultsConfig,
) -> Result<RouteTarget, anyhow::Error> {
    let provider = providers
        .get(&target_cfg.provider)
        .ok_or_else(|| anyhow::anyhow!("Provider '{}' not found in registry", target_cfg.provider))?
        .clone();

    let cb_config = provider_configs
        .iter()
        .find(|p| p.name == target_cfg.provider)
        .and_then(|p| p.circuit_breaker.clone())
        .unwrap_or_else(|| defaults.circuit_breaker.clone());

    Ok(RouteTarget {
        circuit_breaker: Arc::new(CircuitBreaker::new(
            cb_config,
            target_cfg.provider.as_str(),
            model_alias,
        )),
        provider,
        model_id: target_cfg.model_id.clone(),
    })
}

/// Pools over stub providers, for tests that drive selection and dispatch
/// without an upstream.
#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::HashMap;

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::config::Config;
    use crate::error::ProxyError;
    use crate::providers::ProviderStream;
    use crate::types::{ChatCompletionRequest, ChatCompletionResponse};

    /// What a stub provider does with a request.
    #[derive(Clone, Copy)]
    pub(crate) enum Reply {
        /// An upstream 400: an error that does not fail over.
        BadRequest,
        /// An upstream 503: an error that fails over.
        Unavailable,
        /// Never answers.
        Hang,
    }

    struct Stub {
        name: String,
        reply: Reply,
    }

    impl Stub {
        async fn answer<T>(&self) -> Result<T, ProxyError> {
            let status = match self.reply {
                Reply::BadRequest => 400,
                Reply::Unavailable => 503,
                Reply::Hang => std::future::pending().await,
            };
            Err(ProxyError::ProviderError {
                provider: self.name.clone(),
                status,
                message: "stub".to_string(),
            })
        }
    }

    #[async_trait]
    impl ProviderAdapter for Stub {
        fn name(&self) -> &str {
            &self.name
        }

        async fn chat(
            &self,
            _req: &ChatCompletionRequest,
            _model_id: &str,
        ) -> Result<ChatCompletionResponse, ProxyError> {
            self.answer().await
        }

        async fn chat_stream(
            &self,
            _req: &ChatCompletionRequest,
            _model_id: &str,
        ) -> Result<ProviderStream, ProxyError> {
            self.answer().await
        }
    }

    /// A pool named `alias` routing with `strategy`: primaries `p0`, `p1`, …
    /// and fallbacks `f0`, `f1`, …, one stub provider each. Every breaker
    /// opens on its first failure and may be probed again at once.
    pub(crate) fn pool(
        alias: &str,
        strategy: &str,
        primaries: &[Reply],
        fallbacks: &[Reply],
    ) -> (RoutePool, Config) {
        let named = |prefix: &str, replies: &[Reply]| -> Vec<(String, Reply)> {
            replies
                .iter()
                .enumerate()
                .map(|(i, reply)| (format!("{prefix}{i}"), *reply))
                .collect()
        };
        let primaries = named("p", primaries);
        let fallbacks = named("f", fallbacks);
        let target = |(name, _): &(String, Reply)| json!({"provider": name, "model_id": "m"});

        let config: Config = serde_json::from_value(json!({
            "defaults": {
                "retry": {"max_attempts": 1, "initial_backoff_ms": 1,
                          "max_backoff_ms": 1, "jitter": false},
                "circuit_breaker": {"failure_threshold": 1, "success_threshold": 1,
                                    "recovery_timeout_secs": 0}
            },
            "providers": primaries.iter().chain(&fallbacks)
                .map(|(name, _)| json!({"name": name, "type": "openai"}))
                .collect::<Vec<_>>(),
            "models": [{"alias": alias, "routing": {
                "strategy": strategy,
                "targets": primaries.iter().map(target).collect::<Vec<_>>(),
                "fallback": fallbacks.iter().map(target).collect::<Vec<_>>()}}]
        }))
        .unwrap();

        let registry: ProviderRegistry = primaries
            .into_iter()
            .chain(fallbacks)
            .map(|(name, reply)| {
                let stub: Arc<dyn ProviderAdapter> = Arc::new(Stub {
                    name: name.clone(),
                    reply,
                });
                (name, stub)
            })
            .collect::<HashMap<_, _>>();
        let pool = RoutePool::from_config(
            alias,
            config.models[0].routing.as_ref().unwrap(),
            &registry,
            &config.providers,
            &config.defaults,
        )
        .unwrap();
        (pool, config)
    }

    /// Open `target`'s breaker. With no recovery timeout it can be probed
    /// again at once; `half_open` also moves it on to HalfOpen, slot free.
    pub(crate) fn trip(target: &RouteTarget, half_open: bool) {
        target.circuit_breaker.record_failure();
        if half_open {
            drop(target.try_acquire().expect("probe granted"));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;

    use super::circuit_breaker::CircuitState;
    use super::test_support::{pool, trip, Reply};
    use super::*;

    const STRATEGIES: [&str; 4] = ["round_robin", "weighted", "random", "failover"];

    fn claimed(pool: &RoutePool) -> Vec<&str> {
        pool.targets
            .iter()
            .filter(|t| t.circuit_breaker.probe_in_flight())
            .map(|t| t.provider.name())
            .collect()
    }

    #[test]
    fn selecting_claims_the_probe_of_the_returned_target_only() {
        for strategy in STRATEGIES {
            for n in [2, 3] {
                for half_open in [false, true] {
                    let case = format!("{strategy}, {n} primaries, half_open={half_open}");
                    let (pool, _) = pool("lb-claim-one", strategy, &vec![Reply::Hang; n], &[]);
                    pool.targets.iter().for_each(|t| trip(t, half_open));

                    let (target, permit) = pool.select_target(|_| true).expect(&case);
                    assert_eq!(claimed(&pool), [target.provider.name()], "{case}");

                    // The others stay in rotation: each later request probes
                    // a primary nobody is probing yet.
                    let mut held = vec![permit];
                    for _ in 1..n {
                        let (_, permit) = pool.select_target(|_| true).expect(&case);
                        held.push(permit);
                    }
                    assert_eq!(claimed(&pool).len(), n, "{case}");
                    assert!(pool.select_target(|_| true).is_none(), "{case}");

                    // A probe that ends without an outcome frees its slot.
                    drop(held);
                    assert!(claimed(&pool).is_empty(), "{case}");
                    assert!(pool.select_target(|_| true).is_some(), "{case}");
                }
            }
        }
    }

    #[test]
    fn an_unselected_half_open_primary_recovers_on_a_later_request() {
        // Failover keeps preferring the first primary once it has closed, so
        // only the spreading strategies bring every primary back unprompted.
        for strategy in ["round_robin", "weighted", "random"] {
            let (pool, _) = pool("lb-recover", strategy, &[Reply::Hang; 3], &[]);
            pool.targets.iter().for_each(|t| trip(t, true));

            let closed = |t: &RouteTarget| t.circuit_breaker.state() == CircuitState::Closed;
            for _ in 0..500 {
                let (_, permit) = pool.select_target(|_| true).expect(strategy);
                permit.success();
                if pool.targets.iter().all(closed) {
                    break;
                }
            }
            for target in &pool.targets {
                assert!(
                    closed(target),
                    "{strategy}: {} never recovered",
                    target.provider.name()
                );
            }
        }
    }

    #[test]
    fn an_ineligible_half_open_primary_is_not_claimed() {
        for strategy in STRATEGIES {
            let (pool, _) = pool("lb-ineligible", strategy, &[Reply::Hang; 2], &[]);
            pool.targets.iter().for_each(|t| trip(t, true));

            let (target, _permit) = pool
                .select_target(|t| t.provider.name() == "p1")
                .expect(strategy);
            assert_eq!(target.provider.name(), "p1", "{strategy}");
            assert_eq!(claimed(&pool), ["p1"], "{strategy}");

            assert!(pool.select_target(|_| false).is_none(), "{strategy}");
            assert_eq!(claimed(&pool), ["p1"], "{strategy}");
        }
    }

    #[test]
    fn a_closed_pool_selects_without_touching_any_probe_slot() {
        for strategy in STRATEGIES {
            let (pool, _) = pool("lb-closed", strategy, &[Reply::Hang; 3], &[]);
            let (_, permit) = pool.select_target(|_| true).expect(strategy);
            assert!(claimed(&pool).is_empty(), "{strategy}");
            drop(permit);
            for target in &pool.targets {
                assert_eq!(target.circuit_breaker.state(), CircuitState::Closed);
            }
        }
    }

    /// Requests racing for the same half-open primaries: a pick whose slot
    /// another request took in between is ruled out and the strategy picks
    /// again, so every call returns, no target is probed twice at once, and
    /// no free slot is left unused while a request goes unserved.
    #[test]
    fn racing_selections_each_get_a_distinct_probe_or_none() {
        const PRIMARIES: usize = 3;
        const REQUESTS: usize = 8;

        for strategy in STRATEGIES {
            for _ in 0..50 {
                let (pool, _) = pool("lb-race", strategy, &[Reply::Hang; PRIMARIES], &[]);
                pool.targets.iter().for_each(|t| trip(t, true));

                let start = Barrier::new(REQUESTS);
                let hold = Barrier::new(REQUESTS);
                let mut served: Vec<String> = std::thread::scope(|scope| {
                    let handles: Vec<_> = (0..REQUESTS)
                        .map(|_| {
                            scope.spawn(|| {
                                start.wait();
                                let selected = pool.select_target(|_| true);
                                // Keep the permit until every request has selected.
                                hold.wait();
                                selected.map(|(target, _permit)| target.provider.name().to_string())
                            })
                        })
                        .collect();
                    handles
                        .into_iter()
                        .filter_map(|h| h.join().unwrap())
                        .collect()
                });

                served.sort();
                assert_eq!(served, ["p0", "p1", "p2"], "{strategy}");
            }
        }
    }
}
