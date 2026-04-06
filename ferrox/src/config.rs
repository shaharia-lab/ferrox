use anyhow::{bail, Context};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashSet;
use std::env;

// ── Sub-configs ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutsConfig {
    #[serde(default = "default_connect_secs")]
    pub connect_secs: u64,
    #[serde(default = "default_ttfb_secs")]
    pub ttfb_secs: u64,
    #[serde(default = "default_idle_secs")]
    pub idle_secs: u64,
}

fn default_connect_secs() -> u64 {
    10
}
fn default_ttfb_secs() -> u64 {
    60
}
fn default_idle_secs() -> u64 {
    30
}

impl Default for TimeoutsConfig {
    fn default() -> Self {
        Self {
            connect_secs: default_connect_secs(),
            ttfb_secs: default_ttfb_secs(),
            idle_secs: default_idle_secs(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub timeouts: TimeoutsConfig,
    #[serde(default = "default_graceful_shutdown_timeout_secs")]
    pub graceful_shutdown_timeout_secs: u64,
    #[serde(default = "default_max_request_body_bytes")]
    pub max_request_body_bytes: usize,
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}
fn default_port() -> u16 {
    8080
}
fn default_graceful_shutdown_timeout_secs() -> u64 {
    30
}
fn default_max_request_body_bytes() -> usize {
    10 * 1024 * 1024
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            timeouts: TimeoutsConfig::default(),
            graceful_shutdown_timeout_secs: default_graceful_shutdown_timeout_secs(),
            max_request_body_bytes: default_max_request_body_bytes(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_metrics_path")]
    pub path: String,
}

fn default_true() -> bool {
    true
}
fn default_metrics_path() -> String {
    "/metrics".to_string()
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: default_metrics_path(),
        }
    }
}

/// Accepts both a YAML boolean (`true`/`false`) and the strings `"true"`/`"false"`.
/// This is needed because env-var interpolation always produces a string, e.g.
/// `enabled: "${OTEL_ENABLED:-false}"` becomes the string `"false"` after substitution.
fn deserialize_bool_or_string<'de, D>(de: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum BoolOrString {
        Bool(bool),
        Str(String),
    }
    match BoolOrString::deserialize(de)? {
        BoolOrString::Bool(b) => Ok(b),
        BoolOrString::Str(s) => match s.trim() {
            "true" | "1" | "yes" => Ok(true),
            "false" | "0" | "no" | "" => Ok(false),
            other => Err(serde::de::Error::custom(format!(
                "expected boolean or \"true\"/\"false\", got \"{other}\""
            ))),
        },
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracingConfig {
    #[serde(default, deserialize_with = "deserialize_bool_or_string")]
    pub enabled: bool,
    #[serde(default = "default_otlp_endpoint")]
    pub otlp_endpoint: String,
    #[serde(default = "default_service_name")]
    pub service_name: String,
    #[serde(default = "default_service_version")]
    pub service_version: String,
    #[serde(default = "default_sample_rate")]
    pub sample_rate: f64,
}

fn default_otlp_endpoint() -> String {
    "http://otel-collector:4317".to_string()
}
fn default_service_name() -> String {
    "ferrox".to_string()
}
fn default_service_version() -> String {
    "0.1.0".to_string()
}
fn default_sample_rate() -> f64 {
    1.0
}

impl Default for TracingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            otlp_endpoint: default_otlp_endpoint(),
            service_name: default_service_name(),
            service_version: default_service_version(),
            sample_rate: default_sample_rate(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryConfig {
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default = "default_log_format")]
    pub log_format: String,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub tracing: TracingConfig,
}

fn default_log_level() -> String {
    "info".to_string()
}
fn default_log_format() -> String {
    "text".to_string()
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            log_level: default_log_level(),
            log_format: default_log_format(),
            metrics: MetricsConfig::default(),
            tracing: TracingConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_initial_backoff_ms")]
    pub initial_backoff_ms: u64,
    #[serde(default = "default_max_backoff_ms")]
    pub max_backoff_ms: u64,
    #[serde(default = "default_true")]
    pub jitter: bool,
}

fn default_max_attempts() -> u32 {
    3
}
fn default_initial_backoff_ms() -> u64 {
    100
}
fn default_max_backoff_ms() -> u64 {
    2000
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            initial_backoff_ms: default_initial_backoff_ms(),
            max_backoff_ms: default_max_backoff_ms(),
            jitter: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreakerConfig {
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,
    #[serde(default = "default_success_threshold")]
    pub success_threshold: u32,
    #[serde(default = "default_recovery_timeout_secs")]
    pub recovery_timeout_secs: u64,
}

fn default_failure_threshold() -> u32 {
    5
}
fn default_success_threshold() -> u32 {
    2
}
fn default_recovery_timeout_secs() -> u64 {
    30
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: default_failure_threshold(),
            success_threshold: default_success_threshold(),
            recovery_timeout_secs: default_recovery_timeout_secs(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DefaultsConfig {
    #[serde(default)]
    pub timeouts: TimeoutsConfig,
    #[serde(default)]
    pub retry: RetryConfig,
    #[serde(default)]
    pub circuit_breaker: CircuitBreakerConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ProviderType {
    Anthropic,
    OpenAI,
    Gemini,
    Bedrock,
    /// Z.AI GLM — fully OpenAI-compatible; uses OpenAI adapter with a custom base URL.
    Glm,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    #[serde(rename = "type")]
    pub provider_type: ProviderType,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub region: Option<String>,
    pub timeouts: Option<TimeoutsConfig>,
    pub retry: Option<RetryConfig>,
    pub circuit_breaker: Option<CircuitBreakerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RoutingStrategy {
    RoundRobin,
    Weighted,
    Failover,
    Random,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetConfig {
    pub provider: String,
    pub model_id: String,
    pub weight: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingConfig {
    pub strategy: RoutingStrategy,
    pub targets: Vec<TargetConfig>,
    #[serde(default)]
    pub fallback: Vec<TargetConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub alias: String,
    pub routing: RoutingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    pub requests_per_minute: u32,
    pub burst: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualKeyConfig {
    pub key: String,
    pub name: String,
    pub description: Option<String>,
    #[serde(default)]
    pub allowed_models: Vec<String>,
    pub rate_limit: Option<RateLimitConfig>,
}

// ── Rate limiting backend config ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitBackendType {
    #[default]
    Memory,
    Redis,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitingConfig {
    #[serde(default)]
    pub backend: RateLimitBackendType,
    /// Redis URL — required when `backend: redis`
    pub redis_url: Option<String>,
    #[serde(default = "default_redis_key_prefix")]
    pub redis_key_prefix: String,
    #[serde(default = "default_redis_pool_size")]
    pub redis_pool_size: usize,
    /// When true, rate limiting failures (e.g. Redis unavailable) allow the request through.
    /// Default: true
    #[serde(default = "default_true")]
    pub redis_fail_open: bool,
}

fn default_redis_key_prefix() -> String {
    "ferrox:rl:".to_string()
}
fn default_redis_pool_size() -> usize {
    10
}

impl Default for RateLimitingConfig {
    fn default() -> Self {
        Self {
            backend: RateLimitBackendType::Memory,
            redis_url: None,
            redis_key_prefix: default_redis_key_prefix(),
            redis_pool_size: default_redis_pool_size(),
            redis_fail_open: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedIssuerConfig {
    pub issuer: String,
    pub jwks_uri: String,
    #[serde(default)]
    pub audience: Option<String>,
}

// ── Top-level config ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    #[serde(default)]
    pub defaults: DefaultsConfig,
    pub providers: Vec<ProviderConfig>,
    pub models: Vec<ModelConfig>,
    #[serde(default)]
    pub virtual_keys: Vec<VirtualKeyConfig>,
    #[serde(default)]
    pub trusted_issuers: Vec<TrustedIssuerConfig>,
    #[serde(default = "default_jwks_cache_ttl_secs")]
    pub jwks_cache_ttl_secs: u64,
    #[serde(default)]
    pub rate_limiting: RateLimitingConfig,
    /// PostgreSQL connection URL for persisting per-request token usage.
    /// When set, the gateway writes usage records to the `usage_log` table
    /// (shared with ferrox-cp) via an async batched writer.
    /// When absent, usage recording is silently disabled.
    #[serde(default)]
    pub usage_database_url: Option<String>,
}

fn default_jwks_cache_ttl_secs() -> u64 {
    300
}

// ── Loading ──────────────────────────────────────────────────────────────────

pub fn load_config_from(path: &str) -> Result<Config, anyhow::Error> {
    tracing::debug!(path = %path, "Loading config");

    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {path}"))?;

    // Parse YAML into a Value tree first
    let mut value: serde_yaml::Value =
        serde_yaml::from_str(&raw).with_context(|| format!("Failed to parse YAML in {path}"))?;

    // Interpolate env vars in string leaves (safe — no re-parsing)
    interpolate_yaml(&mut value).with_context(|| "Environment variable interpolation failed")?;

    // Deserialize into Config
    let config: Config =
        serde_yaml::from_value(value).with_context(|| "Failed to deserialize config")?;

    validate(&config)?;

    Ok(config)
}

/// Interpolate `${VAR}` and `${VAR:-default}` in all string leaves of a YAML value tree.
/// Operates on the already-parsed Value — never re-parses YAML, so injected values are safe.
fn interpolate_yaml(value: &mut serde_yaml::Value) -> Result<(), anyhow::Error> {
    match value {
        serde_yaml::Value::String(s) => {
            *s = interpolate_env(s)?;
        }
        serde_yaml::Value::Mapping(map) => {
            for v in map.values_mut() {
                interpolate_yaml(v)?;
            }
        }
        serde_yaml::Value::Sequence(seq) => {
            for v in seq.iter_mut() {
                interpolate_yaml(v)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Replace all `${VAR}` and `${VAR:-default}` occurrences in `s`.
pub(crate) fn interpolate_env(s: &str) -> Result<String, anyhow::Error> {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut expr = String::new();
            let mut closed = false;
            for ch in chars.by_ref() {
                if ch == '}' {
                    closed = true;
                    break;
                }
                expr.push(ch);
            }
            if !closed {
                bail!("Unclosed env var reference: ${{{expr}");
            }
            let interpolated = if let Some(pos) = expr.find(":-") {
                let var_name = &expr[..pos];
                let default_val = &expr[pos + 2..];
                env::var(var_name).unwrap_or_else(|_| default_val.to_string())
            } else {
                env::var(&expr)
                    .with_context(|| format!("Required environment variable '{expr}' is not set"))?
            };
            result.push_str(&interpolated);
        } else {
            result.push(c);
        }
    }

    Ok(result)
}

pub(crate) fn validate(config: &Config) -> Result<(), anyhow::Error> {
    // Unique provider names
    let mut provider_names = HashSet::new();
    for p in &config.providers {
        if !provider_names.insert(p.name.clone()) {
            bail!("Duplicate provider name: '{}'", p.name);
        }
    }

    // Unique model aliases
    let mut model_aliases = HashSet::new();
    for m in &config.models {
        if !model_aliases.insert(m.alias.clone()) {
            bail!("Duplicate model alias: '{}'", m.alias);
        }
    }

    // Unique virtual key names
    let mut key_names = HashSet::new();
    for k in &config.virtual_keys {
        if !key_names.insert(k.name.clone()) {
            bail!("Duplicate virtual key name: '{}'", k.name);
        }
    }

    // Validate rate limiting config
    if config.rate_limiting.backend == RateLimitBackendType::Redis
        && config.rate_limiting.redis_url.is_none()
    {
        bail!("rate_limiting.backend is 'redis' but redis_url is not set");
    }

    // Validate model routing references
    for m in &config.models {
        if m.routing.targets.is_empty() {
            bail!("Model '{}' must have at least one target", m.alias);
        }
        if m.routing.strategy == RoutingStrategy::Weighted {
            for t in &m.routing.targets {
                if t.weight.is_none() {
                    bail!(
                        "Model '{}' uses weighted strategy but target '{}' has no weight",
                        m.alias,
                        t.provider
                    );
                }
            }
        }
        for t in m.routing.targets.iter().chain(m.routing.fallback.iter()) {
            if !provider_names.contains(&t.provider) {
                bail!(
                    "Model '{}' references unknown provider '{}'",
                    m.alias,
                    t.provider
                );
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── interpolate_env ───────────────────────────────────────────────────────

    #[test]
    fn interpolate_env_passthrough_plain_string() {
        let result = interpolate_env("hello world").unwrap();
        assert_eq!(result, "hello world");
    }

    #[test]
    fn interpolate_env_substitutes_set_var() {
        std::env::set_var("_FERROX_TEST_VAR", "hello");
        let result = interpolate_env("value=${_FERROX_TEST_VAR}").unwrap();
        std::env::remove_var("_FERROX_TEST_VAR");
        assert_eq!(result, "value=hello");
    }

    #[test]
    fn interpolate_env_uses_default_when_var_unset() {
        std::env::remove_var("_FERROX_MISSING_VAR");
        let result = interpolate_env("${_FERROX_MISSING_VAR:-default_val}").unwrap();
        assert_eq!(result, "default_val");
    }

    #[test]
    fn interpolate_env_prefers_set_var_over_default() {
        std::env::set_var("_FERROX_SET_VAR", "real");
        let result = interpolate_env("${_FERROX_SET_VAR:-fallback}").unwrap();
        std::env::remove_var("_FERROX_SET_VAR");
        assert_eq!(result, "real");
    }

    #[test]
    fn interpolate_env_error_on_missing_required_var() {
        std::env::remove_var("_FERROX_REQUIRED_MISSING");
        let result = interpolate_env("${_FERROX_REQUIRED_MISSING}");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("_FERROX_REQUIRED_MISSING"));
    }

    #[test]
    fn interpolate_env_error_on_unclosed_brace() {
        let result = interpolate_env("${UNCLOSED");
        assert!(result.is_err());
    }

    #[test]
    fn interpolate_env_multiple_refs_in_one_string() {
        std::env::set_var("_FERROX_A", "foo");
        std::env::set_var("_FERROX_B", "bar");
        let result = interpolate_env("${_FERROX_A}-${_FERROX_B}").unwrap();
        std::env::remove_var("_FERROX_A");
        std::env::remove_var("_FERROX_B");
        assert_eq!(result, "foo-bar");
    }

    #[test]
    fn interpolate_env_empty_default_is_valid() {
        std::env::remove_var("_FERROX_EMPTY_DEFAULT");
        let result = interpolate_env("${_FERROX_EMPTY_DEFAULT:-}").unwrap();
        assert_eq!(result, "");
    }

    // ── validate ─────────────────────────────────────────────────────────────

    fn minimal_config(provider_name: &str, alias: &str) -> Config {
        Config {
            server: ServerConfig::default(),
            telemetry: TelemetryConfig::default(),
            defaults: DefaultsConfig::default(),
            providers: vec![ProviderConfig {
                name: provider_name.to_string(),
                provider_type: ProviderType::OpenAI,
                api_key: None,
                base_url: None,
                region: None,
                timeouts: None,
                retry: None,
                circuit_breaker: None,
            }],
            models: vec![ModelConfig {
                alias: alias.to_string(),
                routing: RoutingConfig {
                    strategy: RoutingStrategy::RoundRobin,
                    targets: vec![TargetConfig {
                        provider: provider_name.to_string(),
                        model_id: "test-model".to_string(),
                        weight: None,
                    }],
                    fallback: vec![],
                },
            }],
            virtual_keys: vec![],
            trusted_issuers: vec![],
            jwks_cache_ttl_secs: default_jwks_cache_ttl_secs(),
            rate_limiting: RateLimitingConfig::default(),
            usage_database_url: None,
        }
    }

    #[test]
    fn validate_passes_for_minimal_valid_config() {
        let config = minimal_config("openai", "gpt-4");
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn validate_rejects_duplicate_provider_name() {
        let mut config = minimal_config("openai", "gpt-4");
        config.providers.push(ProviderConfig {
            name: "openai".to_string(),
            provider_type: ProviderType::OpenAI,
            api_key: None,
            base_url: None,
            region: None,
            timeouts: None,
            retry: None,
            circuit_breaker: None,
        });
        let err = validate(&config).unwrap_err().to_string();
        assert!(err.contains("Duplicate provider name"));
    }

    #[test]
    fn validate_rejects_duplicate_model_alias() {
        let mut config = minimal_config("openai", "gpt-4");
        config.models.push(ModelConfig {
            alias: "gpt-4".to_string(),
            routing: RoutingConfig {
                strategy: RoutingStrategy::RoundRobin,
                targets: vec![TargetConfig {
                    provider: "openai".to_string(),
                    model_id: "gpt-4".to_string(),
                    weight: None,
                }],
                fallback: vec![],
            },
        });
        let err = validate(&config).unwrap_err().to_string();
        assert!(err.contains("Duplicate model alias"));
    }

    #[test]
    fn validate_rejects_duplicate_key_names() {
        let mut config = minimal_config("openai", "gpt-4");
        let key = VirtualKeyConfig {
            key: "sk-1".to_string(),
            name: "mykey".to_string(),
            description: None,
            allowed_models: vec!["*".to_string()],
            rate_limit: None,
        };
        config.virtual_keys.push(key.clone());
        config.virtual_keys.push(key);
        let err = validate(&config).unwrap_err().to_string();
        assert!(err.contains("Duplicate virtual key name"));
    }

    #[test]
    fn validate_rejects_model_with_no_targets() {
        let mut config = minimal_config("openai", "gpt-4");
        config.models[0].routing.targets.clear();
        let err = validate(&config).unwrap_err().to_string();
        assert!(err.contains("at least one target"));
    }

    #[test]
    fn validate_rejects_unknown_provider_reference() {
        let mut config = minimal_config("openai", "gpt-4");
        config.models[0].routing.targets[0].provider = "nonexistent".to_string();
        let err = validate(&config).unwrap_err().to_string();
        assert!(err.contains("unknown provider"));
    }

    #[test]
    fn validate_rejects_weighted_target_without_weight() {
        let mut config = minimal_config("openai", "gpt-4");
        config.models[0].routing.strategy = RoutingStrategy::Weighted;
        // target has weight: None — should fail
        let err = validate(&config).unwrap_err().to_string();
        assert!(err.contains("no weight"));
    }

    #[test]
    fn validate_accepts_weighted_targets_with_weights() {
        let mut config = minimal_config("openai", "gpt-4");
        config.models[0].routing.strategy = RoutingStrategy::Weighted;
        config.models[0].routing.targets[0].weight = Some(100);
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn validate_checks_fallback_provider_refs_too() {
        let mut config = minimal_config("openai", "gpt-4");
        config.models[0].routing.fallback.push(TargetConfig {
            provider: "ghost_provider".to_string(),
            model_id: "some-model".to_string(),
            weight: None,
        });
        let err = validate(&config).unwrap_err().to_string();
        assert!(err.contains("unknown provider"));
    }

    // ── rate_limiting validation ──────────────────────────────────────────────

    #[test]
    fn validate_rejects_redis_backend_without_url() {
        let mut config = minimal_config("openai", "gpt-4");
        config.rate_limiting = RateLimitingConfig {
            backend: RateLimitBackendType::Redis,
            redis_url: None,
            ..RateLimitingConfig::default()
        };
        let err = validate(&config).unwrap_err().to_string();
        assert!(err.contains("redis_url"));
    }

    #[test]
    fn validate_accepts_redis_backend_with_url() {
        let mut config = minimal_config("openai", "gpt-4");
        config.rate_limiting = RateLimitingConfig {
            backend: RateLimitBackendType::Redis,
            redis_url: Some("redis://localhost:6379".to_string()),
            ..RateLimitingConfig::default()
        };
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn validate_accepts_memory_backend_without_url() {
        let config = minimal_config("openai", "gpt-4");
        // default backend is memory, no redis_url needed
        assert!(validate(&config).is_ok());
    }
}
