use axum::{extract::State, Json};

use crate::anthropic_types::{AnthropicModelObject, AnthropicModelsResponse};
use crate::state::AppState;

pub async fn list_models_anthropic(State(state): State<AppState>) -> Json<AnthropicModelsResponse> {
    let data: Vec<AnthropicModelObject> = state
        .config
        .models
        .iter()
        .map(|m| AnthropicModelObject {
            object_type: "model".to_string(),
            id: m.alias.clone(),
            display_name: m.alias.clone(),
            created_at: "1970-01-01T00:00:00Z".to_string(),
        })
        .collect();

    let first_id = data.first().map(|m| m.id.clone());
    let last_id = data.last().map(|m| m.id.clone());

    Json(AnthropicModelsResponse {
        data,
        has_more: false,
        first_id,
        last_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        Config, DefaultsConfig, ModelConfig, RateLimitingConfig, RoutingConfig, RoutingStrategy,
        ServerConfig, TelemetryConfig,
    };
    use crate::jwks::JwksCache;
    use crate::metrics::Metrics;
    use crate::providers::ProviderRegistry;
    use crate::ratelimit::MemoryBackend;
    use crate::router::ModelRouter;
    use axum::extract::State;
    use std::collections::HashMap;
    use std::sync::{atomic::AtomicBool, Arc};

    fn build_state(aliases: &[&str]) -> AppState {
        let models = aliases
            .iter()
            .map(|a| ModelConfig {
                alias: a.to_string(),
                routing: RoutingConfig {
                    strategy: RoutingStrategy::RoundRobin,
                    targets: vec![],
                    fallback: vec![],
                },
            })
            .collect();

        let config = Config {
            server: ServerConfig::default(),
            telemetry: TelemetryConfig::default(),
            defaults: DefaultsConfig::default(),
            providers: vec![],
            models,
            virtual_keys: vec![],
            trusted_issuers: vec![],
            jwks_cache_ttl_secs: 300,
            rate_limiting: RateLimitingConfig::default(),
        };

        let registry: ProviderRegistry = HashMap::new();
        let router = ModelRouter::from_config(&config, &registry).unwrap();
        let jwks_cache = JwksCache::new(vec![], 300, reqwest::Client::new());

        AppState {
            rate_limit_backend: Arc::new(MemoryBackend::new()),
            router: Arc::new(router),
            providers: Arc::new(registry),
            metrics: Arc::new(Metrics::new()),
            ready: Arc::new(AtomicBool::new(true)),
            jwks_cache: Arc::new(jwks_cache),
            config: Arc::new(config),
        }
    }

    #[tokio::test]
    async fn returns_all_model_aliases() {
        let state = build_state(&["claude-sonnet", "gpt-4o", "gemini-flash"]);
        let Json(resp) = list_models_anthropic(State(state)).await;
        let ids: Vec<&str> = resp.data.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"claude-sonnet"));
        assert!(ids.contains(&"gpt-4o"));
        assert!(ids.contains(&"gemini-flash"));
    }

    #[tokio::test]
    async fn response_object_type_is_model() {
        let state = build_state(&["gpt-4o"]);
        let Json(resp) = list_models_anthropic(State(state)).await;
        assert_eq!(resp.data[0].object_type, "model");
    }

    #[tokio::test]
    async fn has_more_is_false() {
        let state = build_state(&["gpt-4o"]);
        let Json(resp) = list_models_anthropic(State(state)).await;
        assert!(!resp.has_more);
    }

    #[tokio::test]
    async fn first_and_last_id_set_when_models_present() {
        let state = build_state(&["alpha", "beta"]);
        let Json(resp) = list_models_anthropic(State(state)).await;
        assert!(resp.first_id.is_some());
        assert!(resp.last_id.is_some());
    }

    #[tokio::test]
    async fn first_and_last_id_none_when_no_models() {
        let state = build_state(&[]);
        let Json(resp) = list_models_anthropic(State(state)).await;
        assert!(resp.first_id.is_none());
        assert!(resp.last_id.is_none());
    }
}
