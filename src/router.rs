use std::collections::HashMap;
use std::sync::Arc;

use crate::config::Config;
use crate::error::ProxyError;
use crate::lb::RoutePool;
use crate::providers::ProviderRegistry;

pub struct ModelRouter {
    pools: HashMap<String, Arc<RoutePool>>,
}

impl ModelRouter {
    pub fn from_config(
        config: &Config,
        providers: &ProviderRegistry,
    ) -> Result<Self, anyhow::Error> {
        let mut pools = HashMap::new();

        for model in &config.models {
            let pool =
                RoutePool::from_config(model, providers, &config.providers, &config.defaults)?;
            pools.insert(model.alias.clone(), Arc::new(pool));
        }

        Ok(Self { pools })
    }

    /// Resolve a model alias to its RoutePool.
    pub fn resolve(&self, alias: &str) -> Result<Arc<RoutePool>, ProxyError> {
        self.pools.get(alias).cloned().ok_or_else(|| {
            ProxyError::ModelNotFound(format!("Model alias '{}' is not configured", alias))
        })
    }

    /// List all configured model aliases.
    #[allow(dead_code)]
    pub fn model_aliases(&self) -> Vec<&str> {
        self.pools.keys().map(|s| s.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use async_trait::async_trait;

    use crate::config::{
        CircuitBreakerConfig, DefaultsConfig, ModelConfig, ProviderConfig, ProviderType,
        RetryConfig, RoutingConfig, RoutingStrategy, TargetConfig, TimeoutsConfig,
    };
    use crate::error::ProxyError;
    use crate::providers::{ProviderAdapter, ProviderRegistry, ProviderStream};
    use crate::types::{ChatCompletionRequest, ChatCompletionResponse};

    struct MockProvider;

    #[async_trait]
    impl ProviderAdapter for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }

        async fn chat(
            &self,
            _req: &ChatCompletionRequest,
            _model_id: &str,
        ) -> Result<ChatCompletionResponse, ProxyError> {
            unimplemented!("mock: not used in router tests")
        }

        async fn chat_stream(
            &self,
            _req: &ChatCompletionRequest,
            _model_id: &str,
        ) -> Result<ProviderStream, ProxyError> {
            unimplemented!("mock: not used in router tests")
        }
    }

    fn mock_registry() -> ProviderRegistry {
        let mut reg = ProviderRegistry::new();
        reg.insert(
            "mock".to_string(),
            Arc::new(MockProvider) as Arc<dyn ProviderAdapter>,
        );
        reg
    }

    fn mock_config(aliases: &[&str]) -> Config {
        Config {
            server: crate::config::ServerConfig::default(),
            telemetry: crate::config::TelemetryConfig::default(),
            defaults: DefaultsConfig {
                timeouts: TimeoutsConfig::default(),
                retry: RetryConfig::default(),
                circuit_breaker: CircuitBreakerConfig::default(),
            },
            providers: vec![ProviderConfig {
                name: "mock".to_string(),
                provider_type: ProviderType::OpenAI,
                api_key: None,
                base_url: None,
                region: None,
                timeouts: None,
                retry: None,
                circuit_breaker: None,
            }],
            models: aliases
                .iter()
                .map(|alias| ModelConfig {
                    alias: alias.to_string(),
                    routing: RoutingConfig {
                        strategy: RoutingStrategy::RoundRobin,
                        targets: vec![TargetConfig {
                            provider: "mock".to_string(),
                            model_id: "mock-v1".to_string(),
                            weight: None,
                        }],
                        fallback: vec![],
                    },
                })
                .collect(),
            virtual_keys: vec![],
            trusted_issuers: vec![],
            jwks_cache_ttl_secs: 300,
            rate_limiting: crate::config::RateLimitingConfig::default(),
        }
    }

    #[test]
    fn resolve_returns_pool_for_known_alias() {
        let config = mock_config(&["gpt-4", "claude-3"]);
        let reg = mock_registry();
        let router = ModelRouter::from_config(&config, &reg).unwrap();

        assert!(router.resolve("gpt-4").is_ok());
        assert!(router.resolve("claude-3").is_ok());
    }

    #[test]
    fn resolve_returns_model_not_found_for_unknown_alias() {
        let config = mock_config(&["gpt-4"]);
        let reg = mock_registry();
        let router = ModelRouter::from_config(&config, &reg).unwrap();

        let result = router.resolve("nonexistent");
        assert!(result.is_err());
        match result {
            Err(ProxyError::ModelNotFound(msg)) => assert!(msg.contains("nonexistent")),
            Err(e) => panic!("unexpected error: {e}"),
            Ok(_) => panic!("expected Err but got Ok"),
        }
    }

    #[test]
    fn model_aliases_returns_all_configured_aliases() {
        let config = mock_config(&["alias-a", "alias-b", "alias-c"]);
        let reg = mock_registry();
        let router = ModelRouter::from_config(&config, &reg).unwrap();

        let mut aliases = router.model_aliases();
        aliases.sort();
        assert_eq!(aliases, vec!["alias-a", "alias-b", "alias-c"]);
    }

    #[test]
    fn from_config_fails_when_provider_not_in_registry() {
        let mut config = mock_config(&["gpt-4"]);
        config.models[0].routing.targets[0].provider = "missing_provider".to_string();
        // Also update providers to avoid validate() rejecting it, but here
        // we bypass validate() and test build_target() failure directly.
        config.providers.push(ProviderConfig {
            name: "missing_provider".to_string(),
            provider_type: ProviderType::OpenAI,
            api_key: None,
            base_url: None,
            region: None,
            timeouts: None,
            retry: None,
            circuit_breaker: None,
        });

        let reg = mock_registry(); // registry only has "mock"
        let result = ModelRouter::from_config(&config, &reg);
        assert!(result.is_err());
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .contains("missing_provider"));
    }
}
