use anyhow::{bail, Context};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::env;

/// The provider-facing slice of the configuration lives in `ferrox-providers`
/// so adapters can be built without depending on the gateway. Re-exported here
/// so `config::ProviderConfig` and friends keep resolving across the crate.
///
/// The full set is re-exported to keep every pre-extraction `config::*` path
/// valid; `ferrox` is a binary, so the ones it happens not to reference itself
/// (or references only from tests) would otherwise read as unused.
#[allow(unused_imports)]
pub use ferrox_providers::config::{
    AwsAssumeRoleConfig, AwsAuthConfig, AwsConfig, CircuitBreakerConfig, DefaultsConfig,
    ProviderConfig, ProviderType, ResponsesMode, RetryConfig, TimeoutsConfig,
};

// ── Sub-configs ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(
        default = "default_port",
        deserialize_with = "deserialize_u16_or_string"
    )]
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

/// Accepts both a YAML integer and a string representation of one.
/// This is needed because env-var interpolation always produces a string, e.g.
/// `port: "${FERROX_PORT:-8080}"` becomes the string `"8080"` after substitution.
fn deserialize_u16_or_string<'de, D>(de: D) -> Result<u16, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum U16OrString {
        Int(u16),
        Str(String),
    }
    match U16OrString::deserialize(de)? {
        U16OrString::Int(n) => Ok(n),
        U16OrString::Str(s) => s.trim().parse::<u16>().map_err(|_| {
            serde::de::Error::custom(format!("expected a port number (0–65535), got \"{s}\""))
        }),
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

/// A model alias is resolved either statically (`routing`) or by a classifier
/// that picks one of several statically routed aliases (`classifier`).
/// Exactly one of the two must be set; `validate()` enforces it so the error
/// can name the alias.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub alias: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<RoutingConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier: Option<ClassifiedAliasConfig>,
}

/// The `classifier` form of a model alias: which classifier decides, the
/// tiers it may choose between, and where the request goes when it cannot
/// decide.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassifiedAliasConfig {
    /// Id of an entry in the top-level `classifiers` list.
    #[serde(rename = "use")]
    pub classifier: String,
    /// Tier name → the statically routed alias that serves it.
    pub tiers: BTreeMap<String, TierConfig>,
    /// Statically routed alias used whenever the classifier gives no usable
    /// answer (error, timeout, low confidence).
    pub fallback_alias: String,
    /// Answers below this confidence go to `fallback_alias`. 0 accepts all.
    #[serde(default)]
    pub confidence_threshold: f64,
    /// When true the classifier runs and its choice is recorded, but the
    /// request is still served by `fallback_alias`.
    #[serde(default)]
    pub shadow: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierConfig {
    /// Statically routed alias that serves requests classified into this tier.
    pub alias: String,
    /// Plain-language description of when this tier applies; shown to the
    /// classifier as the option's description.
    pub when: String,
}

// ── Classifiers ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClassifierType {
    Jev,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ClassifierConfig {
    /// Unique id; a classified alias refers to it with `use`.
    pub id: String,
    #[serde(rename = "type")]
    pub classifier_type: ClassifierType,
    pub api_key: String,
    #[serde(default = "default_classifier_model")]
    pub model: String,
    #[serde(default = "default_classifier_base_url")]
    pub base_url: String,
    #[serde(default = "default_classifier_timeout_ms")]
    pub timeout_ms: u64,
    /// Breaker around the classifier itself. Falls back to
    /// `defaults.circuit_breaker` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub circuit_breaker: Option<CircuitBreakerConfig>,
    /// Upper bound on the request text sent to the classifier.
    #[serde(default = "default_classifier_max_input_chars")]
    pub max_input_chars: usize,
    /// How long a decision is reused for an identical input. 0 disables the
    /// cache.
    #[serde(default = "default_classifier_cache_ttl_secs")]
    pub cache_ttl_secs: u64,
    #[serde(default = "default_classifier_cache_max_entries")]
    pub cache_max_entries: usize,
}

fn default_classifier_model() -> String {
    "jev-latest".to_string()
}
fn default_classifier_base_url() -> String {
    "https://api.typesafe.ai".to_string()
}
fn default_classifier_timeout_ms() -> u64 {
    500
}
fn default_classifier_max_input_chars() -> usize {
    8000
}
fn default_classifier_cache_ttl_secs() -> u64 {
    300
}
fn default_classifier_cache_max_entries() -> usize {
    10_000
}

impl std::fmt::Debug for ClassifierConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClassifierConfig")
            .field("id", &self.id)
            .field("classifier_type", &self.classifier_type)
            .field("api_key", &"[REDACTED]")
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("timeout_ms", &self.timeout_ms)
            .field("circuit_breaker", &self.circuit_breaker)
            .field("max_input_chars", &self.max_input_chars)
            .field("cache_ttl_secs", &self.cache_ttl_secs)
            .field("cache_max_entries", &self.cache_max_entries)
            .finish()
    }
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
    /// Classifiers that classified aliases (`models[].classifier`) refer to.
    #[serde(default)]
    pub classifiers: Vec<ClassifierConfig>,
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
    /// Webhook endpoints that receive async push notifications for events
    /// like token usage.  Each endpoint is called via HTTP POST with a
    /// JSON payload and Bearer token authentication.
    #[serde(default)]
    pub event_endpoints: Vec<EventEndpointConfig>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct EventEndpointConfig {
    /// Unique name for this endpoint (used in logs and metrics).
    pub name: String,
    /// HTTP(S) URL to POST events to.
    pub url: String,
    /// Bearer token for authenticating outgoing webhook requests.
    pub token: String,
    /// Event types this endpoint subscribes to (e.g., `["token_usage"]`).
    pub events: Vec<String>,
}

impl std::fmt::Debug for EventEndpointConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventEndpointConfig")
            .field("name", &self.name)
            .field("url", &self.url)
            .field("token", &"[REDACTED]")
            .field("events", &self.events)
            .finish()
    }
}

fn default_jwks_cache_ttl_secs() -> u64 {
    300
}

// ── Loading ──────────────────────────────────────────────────────────────────

pub fn load_config_from(path: &str) -> Result<Config, anyhow::Error> {
    tracing::debug!(path = %path, "Loading config");

    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {path}"))?;

    parse_config(&raw).with_context(|| format!("Failed to load config from {path}"))
}

fn parse_config(raw: &str) -> Result<Config, anyhow::Error> {
    // Parse YAML into a Value tree first
    let mut value: serde_yaml::Value =
        serde_yaml::from_str(raw).with_context(|| "Failed to parse YAML")?;

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

    // Validate the metrics route path: it is mounted verbatim on the router,
    // and axum panics at startup on a path that doesn't start with '/'. Fail
    // here with a clear message instead.
    if config.telemetry.metrics.enabled && !config.telemetry.metrics.path.starts_with('/') {
        bail!(
            "telemetry.metrics.path must start with '/', got '{}'",
            config.telemetry.metrics.path
        );
    }

    // Unique classifier ids
    let mut classifier_ids = HashSet::new();
    for c in &config.classifiers {
        if !classifier_ids.insert(c.id.as_str()) {
            bail!("Duplicate classifier id: '{}'", c.id);
        }
    }

    // Every alias is either statically routed or classified, never both.
    let mut static_aliases = HashSet::new();
    for m in &config.models {
        match (&m.routing, &m.classifier) {
            (Some(_), None) => {
                static_aliases.insert(m.alias.as_str());
            }
            (None, Some(_)) => {}
            (Some(_), Some(_)) => bail!(
                "Model '{}' sets both 'routing' and 'classifier'; exactly one is required",
                m.alias
            ),
            (None, None) => bail!(
                "Model '{}' sets neither 'routing' nor 'classifier'; exactly one is required",
                m.alias
            ),
        }
    }

    // Validate model routing references
    for m in &config.models {
        let Some(routing) = &m.routing else { continue };
        if routing.targets.is_empty() {
            bail!("Model '{}' must have at least one target", m.alias);
        }
        if routing.strategy == RoutingStrategy::Weighted {
            for t in &routing.targets {
                if t.weight.is_none() {
                    bail!(
                        "Model '{}' uses weighted strategy but target '{}' has no weight",
                        m.alias,
                        t.provider
                    );
                }
            }
        }
        for t in routing.targets.iter().chain(routing.fallback.iter()) {
            if !provider_names.contains(&t.provider) {
                bail!(
                    "Model '{}' references unknown provider '{}'",
                    m.alias,
                    t.provider
                );
            }
        }
    }

    // Validate classified aliases. Tiers and the fallback must be statically
    // routed, so a classified alias can never chain or cycle into another.
    for m in &config.models {
        let Some(classified) = &m.classifier else {
            continue;
        };
        if !classifier_ids.contains(classified.classifier.as_str()) {
            bail!(
                "Model '{}' uses unknown classifier '{}'",
                m.alias,
                classified.classifier
            );
        }
        if classified.tiers.is_empty() {
            bail!("Model '{}' must have at least one classifier tier", m.alias);
        }
        let targets = classified
            .tiers
            .iter()
            .map(|(name, tier)| (format!("tier '{name}'"), tier.alias.as_str()))
            .chain([(
                "fallback_alias".to_string(),
                classified.fallback_alias.as_str(),
            )]);
        for (what, alias) in targets {
            if !model_aliases.contains(alias) {
                bail!(
                    "Model '{}' {what} references unknown alias '{alias}'",
                    m.alias
                );
            }
            if !static_aliases.contains(alias) {
                bail!(
                    "Model '{}' {what} references classified alias '{alias}'; it must be a statically routed alias",
                    m.alias
                );
            }
        }
        // Written so NaN is rejected too.
        if !(0.0..=1.0).contains(&classified.confidence_threshold) {
            bail!(
                "Model '{}' confidence_threshold must be between 0 and 1, got {}",
                m.alias,
                classified.confidence_threshold
            );
        }
    }

    // Native /responses passthrough needs an upstream that speaks the
    // Responses API; only a generic OpenAI-protocol provider can.
    for p in &config.providers {
        if p.responses == ResponsesMode::Native && p.provider_type != ProviderType::OpenAI {
            bail!(
                "Provider '{}': 'responses: native' is only supported for type 'openai'",
                p.name
            );
        }
    }

    // Validate AWS credential configuration for Bedrock providers.
    for p in &config.providers {
        if let Some(auth) = p.aws.as_ref().and_then(|a| a.auth.as_ref()) {
            let has_static = auth.access_key_id.is_some() || auth.secret_access_key.is_some();
            if auth.access_key_id.is_some() != auth.secret_access_key.is_some() {
                bail!(
                    "Provider '{}': aws.auth requires both access_key_id and secret_access_key together",
                    p.name
                );
            }
            if has_static && auth.profile.is_some() {
                bail!(
                    "Provider '{}': aws.auth sets both static credentials and a profile; use exactly one base source",
                    p.name
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
                aws: None,
                timeouts: None,
                circuit_breaker: None,
                responses: Default::default(),
            }],
            models: vec![ModelConfig {
                alias: alias.to_string(),
                classifier: None,
                routing: Some(RoutingConfig {
                    strategy: RoutingStrategy::RoundRobin,
                    targets: vec![TargetConfig {
                        provider: provider_name.to_string(),
                        model_id: "test-model".to_string(),
                        weight: None,
                    }],
                    fallback: vec![],
                }),
            }],
            classifiers: vec![],
            virtual_keys: vec![],
            trusted_issuers: vec![],
            jwks_cache_ttl_secs: default_jwks_cache_ttl_secs(),
            rate_limiting: RateLimitingConfig::default(),
            usage_database_url: None,
            event_endpoints: vec![],
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
            aws: None,
            timeouts: None,
            circuit_breaker: None,
            responses: Default::default(),
        });
        let err = validate(&config).unwrap_err().to_string();
        assert!(err.contains("Duplicate provider name"));
    }

    #[test]
    fn validate_allows_native_responses_only_on_openai_providers() {
        let mut config = minimal_config("openai", "gpt-4");
        config.providers[0].responses = ResponsesMode::Native;
        assert!(validate(&config).is_ok());

        config.providers[0].provider_type = ProviderType::Glm;
        let err = validate(&config).unwrap_err().to_string();
        assert!(
            err.contains("'responses: native' is only supported"),
            "{err}"
        );
    }

    #[test]
    fn validate_rejects_duplicate_model_alias() {
        let mut config = minimal_config("openai", "gpt-4");
        config.models.push(ModelConfig {
            alias: "gpt-4".to_string(),
            classifier: None,
            routing: Some(RoutingConfig {
                strategy: RoutingStrategy::RoundRobin,
                targets: vec![TargetConfig {
                    provider: "openai".to_string(),
                    model_id: "gpt-4".to_string(),
                    weight: None,
                }],
                fallback: vec![],
            }),
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
        config.models[0].routing.as_mut().unwrap().targets.clear();
        let err = validate(&config).unwrap_err().to_string();
        assert!(err.contains("at least one target"));
    }

    #[test]
    fn validate_rejects_metrics_path_without_leading_slash() {
        let mut config = minimal_config("openai", "gpt-4");
        config.telemetry.metrics.path = "metrics".to_string();
        let err = validate(&config).unwrap_err().to_string();
        assert!(err.contains("telemetry.metrics.path must start with '/'"));
    }

    #[test]
    fn validate_ignores_metrics_path_when_disabled() {
        let mut config = minimal_config("openai", "gpt-4");
        config.telemetry.metrics.enabled = false;
        config.telemetry.metrics.path = "not-a-path".to_string();
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn validate_rejects_unknown_provider_reference() {
        let mut config = minimal_config("openai", "gpt-4");
        config.models[0].routing.as_mut().unwrap().targets[0].provider = "nonexistent".to_string();
        let err = validate(&config).unwrap_err().to_string();
        assert!(err.contains("unknown provider"));
    }

    #[test]
    fn validate_rejects_weighted_target_without_weight() {
        let mut config = minimal_config("openai", "gpt-4");
        config.models[0].routing.as_mut().unwrap().strategy = RoutingStrategy::Weighted;
        // target has weight: None — should fail
        let err = validate(&config).unwrap_err().to_string();
        assert!(err.contains("no weight"));
    }

    #[test]
    fn validate_accepts_weighted_targets_with_weights() {
        let mut config = minimal_config("openai", "gpt-4");
        config.models[0].routing.as_mut().unwrap().strategy = RoutingStrategy::Weighted;
        config.models[0].routing.as_mut().unwrap().targets[0].weight = Some(100);
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn validate_checks_fallback_provider_refs_too() {
        let mut config = minimal_config("openai", "gpt-4");
        config.models[0]
            .routing
            .as_mut()
            .unwrap()
            .fallback
            .push(TargetConfig {
                provider: "ghost_provider".to_string(),
                model_id: "some-model".to_string(),
                weight: None,
            });
        let err = validate(&config).unwrap_err().to_string();
        assert!(err.contains("unknown provider"));
    }

    // ── classifiers and classified aliases ───────────────────────────────────

    /// The example from the epic's design comment (#170).
    const CLASSIFIED_YAML: &str = r#"
providers:
  - name: openai
    type: openai
classifiers:
  - id: jev-main
    type: jev
    api_key: "${_FERROX_TEST_TYPESAFE_KEY:-ts-secret}"
    model: jev-latest
    timeout_ms: 500
models:
  - alias: fast
    routing: { strategy: failover, targets: [{ provider: openai, model_id: small }] }
  - alias: smart
    routing: { strategy: failover, targets: [{ provider: openai, model_id: large }] }
  - alias: auto
    classifier:
      use: jev-main
      confidence_threshold: 0.6
      fallback_alias: smart
      tiers:
        simple:  { alias: fast,  when: "Short factual or chat" }
        complex: { alias: smart, when: "Multi-step reasoning or code" }
"#;

    fn classified_config() -> Config {
        parse_config(CLASSIFIED_YAML).unwrap()
    }

    /// The classified alias (`auto`) of [`classified_config`].
    fn auto(config: &mut Config) -> &mut ClassifiedAliasConfig {
        config.models[2].classifier.as_mut().unwrap()
    }

    #[test]
    fn classified_alias_example_loads() {
        let mut config = classified_config();

        let classifier = &config.classifiers[0];
        assert_eq!(classifier.id, "jev-main");
        assert_eq!(classifier.classifier_type, ClassifierType::Jev);
        assert_eq!(classifier.api_key, "ts-secret");
        assert_eq!(classifier.model, "jev-latest");
        assert_eq!(classifier.timeout_ms, 500);

        assert!(config.models[0].classifier.is_none());
        assert!(config.models[2].routing.is_none());
        let auto = auto(&mut config);
        assert_eq!(auto.classifier, "jev-main");
        assert_eq!(auto.fallback_alias, "smart");
        assert_eq!(auto.confidence_threshold, 0.6);
        assert!(!auto.shadow);
        assert_eq!(auto.tiers["simple"].alias, "fast");
        assert_eq!(auto.tiers["complex"].when, "Multi-step reasoning or code");
    }

    #[test]
    fn classifier_defaults_apply_when_only_required_fields_are_set() {
        let classifier: ClassifierConfig =
            serde_yaml::from_str("{ id: c, type: jev, api_key: k }").unwrap();
        assert_eq!(classifier.model, "jev-latest");
        assert_eq!(classifier.base_url, "https://api.typesafe.ai");
        assert_eq!(classifier.timeout_ms, 500);
        assert!(classifier.circuit_breaker.is_none());
        assert_eq!(classifier.max_input_chars, 8000);
        assert_eq!(classifier.cache_ttl_secs, 300);
        assert_eq!(classifier.cache_max_entries, 10_000);

        let classified: ClassifiedAliasConfig = serde_yaml::from_str(
            "{ use: c, fallback_alias: a, tiers: { t: { alias: a, when: w } } }",
        )
        .unwrap();
        assert_eq!(classified.confidence_threshold, 0.0);
        assert!(!classified.shadow);
    }

    #[test]
    fn classifier_type_rejects_unknown_backend() {
        let err = serde_yaml::from_str::<ClassifierConfig>("{ id: c, type: llm, api_key: k }")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown variant `llm`"), "{err}");
    }

    #[test]
    fn config_without_classifiers_loads_unchanged() {
        let config = parse_config(
            r#"
providers:
  - name: openai
    type: openai
models:
  - alias: gpt-4
    routing:
      strategy: round_robin
      targets: [{ provider: openai, model_id: gpt-4 }]
"#,
        )
        .unwrap();
        assert!(config.classifiers.is_empty());
        assert!(config.models[0].classifier.is_none());
        assert_eq!(
            config.models[0].routing.as_ref().unwrap().targets[0].model_id,
            "gpt-4"
        );
    }

    #[test]
    fn classifier_debug_redacts_api_key() {
        let config = classified_config();
        let debug = format!("{config:?}");
        assert!(!debug.contains("ts-secret"), "{debug}");
        assert!(debug.contains("[REDACTED]"));
        assert!(debug.contains("jev-main"));
    }

    #[test]
    fn validate_rejects_duplicate_classifier_id() {
        let mut config = classified_config();
        config.classifiers.push(config.classifiers[0].clone());
        let err = validate(&config).unwrap_err().to_string();
        assert_eq!(err, "Duplicate classifier id: 'jev-main'");
    }

    #[test]
    fn validate_rejects_alias_with_both_routing_and_classifier() {
        let mut config = classified_config();
        config.models[2].routing = config.models[0].routing.clone();
        let err = validate(&config).unwrap_err().to_string();
        assert!(err.contains("Model 'auto' sets both"), "{err}");
    }

    #[test]
    fn validate_rejects_alias_with_neither_routing_nor_classifier() {
        let mut config = classified_config();
        config.models[2].classifier = None;
        let err = validate(&config).unwrap_err().to_string();
        assert!(err.contains("Model 'auto' sets neither"), "{err}");
    }

    #[test]
    fn validate_rejects_unknown_classifier() {
        let mut config = classified_config();
        auto(&mut config).classifier = "ghost".to_string();
        let err = validate(&config).unwrap_err().to_string();
        assert_eq!(err, "Model 'auto' uses unknown classifier 'ghost'");
    }

    #[test]
    fn validate_rejects_empty_tiers() {
        let mut config = classified_config();
        auto(&mut config).tiers.clear();
        let err = validate(&config).unwrap_err().to_string();
        assert_eq!(err, "Model 'auto' must have at least one classifier tier");
    }

    #[test]
    fn validate_rejects_tier_naming_missing_alias() {
        let mut config = classified_config();
        auto(&mut config).tiers.get_mut("simple").unwrap().alias = "ghost".to_string();
        let err = validate(&config).unwrap_err().to_string();
        assert_eq!(
            err,
            "Model 'auto' tier 'simple' references unknown alias 'ghost'"
        );
    }

    #[test]
    fn validate_rejects_tier_naming_classified_alias() {
        let mut config = classified_config();
        let mut other = config.models[2].clone();
        other.alias = "auto-2".to_string();
        config.models.push(other);
        auto(&mut config).tiers.get_mut("complex").unwrap().alias = "auto-2".to_string();
        let err = validate(&config).unwrap_err().to_string();
        assert!(
            err.starts_with("Model 'auto' tier 'complex' references classified alias 'auto-2'"),
            "{err}"
        );
    }

    #[test]
    fn validate_rejects_tier_naming_its_own_alias() {
        let mut config = classified_config();
        auto(&mut config).tiers.get_mut("simple").unwrap().alias = "auto".to_string();
        let err = validate(&config).unwrap_err().to_string();
        assert!(
            err.starts_with("Model 'auto' tier 'simple' references classified alias 'auto'"),
            "{err}"
        );
    }

    #[test]
    fn validate_rejects_fallback_naming_missing_alias() {
        let mut config = classified_config();
        auto(&mut config).fallback_alias = "ghost".to_string();
        let err = validate(&config).unwrap_err().to_string();
        assert_eq!(
            err,
            "Model 'auto' fallback_alias references unknown alias 'ghost'"
        );
    }

    #[test]
    fn validate_rejects_fallback_naming_classified_alias() {
        let mut config = classified_config();
        auto(&mut config).fallback_alias = "auto".to_string();
        let err = validate(&config).unwrap_err().to_string();
        assert!(
            err.starts_with("Model 'auto' fallback_alias references classified alias 'auto'"),
            "{err}"
        );
    }

    #[test]
    fn validate_rejects_confidence_threshold_outside_unit_range() {
        for bad in [-0.1, 1.1, f64::NAN] {
            let mut config = classified_config();
            auto(&mut config).confidence_threshold = bad;
            let err = validate(&config).unwrap_err().to_string();
            assert!(
                err.starts_with("Model 'auto' confidence_threshold must be between 0 and 1"),
                "{bad}: {err}"
            );
        }
        for ok in [0.0, 1.0] {
            let mut config = classified_config();
            auto(&mut config).confidence_threshold = ok;
            assert!(validate(&config).is_ok(), "{ok}");
        }
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
