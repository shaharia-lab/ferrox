pub mod circuit_breaker;
pub mod strategy;

use std::sync::Arc;

use crate::config::{DefaultsConfig, ModelConfig, ProviderConfig, RoutingStrategy};
use crate::providers::{ProviderAdapter, ProviderRegistry};
use crate::telemetry::metrics::ROUTING_TARGET_SELECTED;

use circuit_breaker::CircuitBreaker;
use strategy::LbStrategy;

// ── RouteTarget ───────────────────────────────────────────────────────────────

pub struct RouteTarget {
    pub provider: Arc<dyn ProviderAdapter>,
    pub model_id: String,
    pub circuit_breaker: Arc<CircuitBreaker>,
}

impl RouteTarget {
    pub fn is_available(&self) -> bool {
        self.circuit_breaker.is_available()
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
        model: &ModelConfig,
        providers: &ProviderRegistry,
        provider_configs: &[ProviderConfig],
        defaults: &DefaultsConfig,
    ) -> Result<Self, anyhow::Error> {
        let routing = &model.routing;

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
            .map(|t| build_target(t, &model.alias, providers, provider_configs, defaults))
            .collect::<Result<Vec<_>, _>>()?;

        let fallbacks = routing
            .fallback
            .iter()
            .map(|t| build_target(t, &model.alias, providers, provider_configs, defaults))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(RoutePool {
            alias: model.alias.clone(),
            strategy,
            strategy_name,
            targets,
            fallbacks,
        })
    }

    /// Select the best available primary target.
    /// Also records the `routing_target_selected` metric.
    pub fn select_target(&self) -> Option<&RouteTarget> {
        let available: Vec<bool> = self.targets.iter().map(|t| t.is_available()).collect();
        let idx = self.strategy.select(&available)?;
        let target = &self.targets[idx];

        ROUTING_TARGET_SELECTED
            .with_label_values(&[
                self.alias.as_str(),
                target.provider.name(),
                self.strategy_name,
            ])
            .inc();

        Some(target)
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
