//! Configuration types the provider adapters are constructed from.
//!
//! These are the sub-tree of Ferrox's YAML configuration that the translation
//! layer needs — plain serde data with no behaviour attached. They live here
//! rather than in the gateway so a consumer can build an adapter without
//! depending on `ferrox` itself.
//!
//! [`RetryConfig`] and [`CircuitBreakerConfig`] are never read by this crate.
//! They are carried because [`DefaultsConfig`] is deserialized from a single
//! YAML object that also holds that gateway policy, and splitting the struct
//! would silently drop it on round-trip.

use serde::{Deserialize, Serialize};

// ── Timeouts ─────────────────────────────────────────────────────────────────

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

// ── Retry (gateway policy — carried, not read here) ──────────────────────────

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
fn default_true() -> bool {
    true
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

// ── Circuit breaker (gateway policy — carried, not read here) ────────────────

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

// ── Defaults ─────────────────────────────────────────────────────────────────

/// Fallbacks applied to every provider that does not override them.
///
/// Only [`timeouts`](Self::timeouts) is consumed by the adapters.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DefaultsConfig {
    #[serde(default)]
    pub timeouts: TimeoutsConfig,
    #[serde(default)]
    pub retry: RetryConfig,
    #[serde(default)]
    pub circuit_breaker: CircuitBreakerConfig,
}

// ── Providers ────────────────────────────────────────────────────────────────

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
    /// AWS-specific configuration (region + credentials). Used by the `bedrock`
    /// provider; ignored by every other provider type.
    pub aws: Option<AwsConfig>,
    pub timeouts: Option<TimeoutsConfig>,
    pub circuit_breaker: Option<CircuitBreakerConfig>,
}

/// AWS configuration for a `bedrock` provider: the region and how to obtain
/// credentials. When `auth` is omitted, the standard AWS default credential
/// chain is used (environment variables, `~/.aws`, SSO cache, and EC2/ECS/EKS
/// instance roles) — the same behaviour as the AWS CLI/SDKs.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AwsConfig {
    /// AWS region, e.g. `us-east-1`. Falls back to the default chain
    /// (`AWS_REGION`, profile) when omitted.
    pub region: Option<String>,
    /// Explicit credential source. Omit to use the default credential chain.
    pub auth: Option<AwsAuthConfig>,
    /// Override the Bedrock runtime endpoint (VPC endpoints, testing). Rarely
    /// needed; the SDK derives the correct regional endpoint otherwise.
    pub endpoint_url: Option<String>,
}

/// How Ferrox obtains AWS credentials for a Bedrock provider. Exactly one base
/// source may be set (static keys **or** a named profile); leaving both unset
/// uses the default credential chain. `assume_role` optionally layers an STS
/// AssumeRole on top of whichever base source is resolved.
///
/// Secrets should be supplied via environment-variable interpolation
/// (`${AWS_SECRET_ACCESS_KEY}`) rather than inline literals.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AwsAuthConfig {
    /// Static access key id. Requires `secret_access_key`.
    pub access_key_id: Option<String>,
    /// Static secret access key. Requires `access_key_id`.
    pub secret_access_key: Option<String>,
    /// Optional session token, for temporary/STS-issued static credentials.
    pub session_token: Option<String>,
    /// Named profile from `~/.aws/config` / `~/.aws/credentials` (incl. SSO and
    /// `credential_process`). Mutually exclusive with the static keys above.
    pub profile: Option<String>,
    /// Optionally assume an IAM role (STS) on top of the resolved base source.
    pub assume_role: Option<AwsAssumeRoleConfig>,
}

/// STS AssumeRole parameters. The base credentials (static, profile, or default
/// chain) are used to call STS; the resulting temporary credentials are
/// auto-refreshed by the SDK.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AwsAssumeRoleConfig {
    /// ARN of the role to assume, e.g. `arn:aws:iam::123456789012:role/ferrox`.
    pub role_arn: String,
    /// Role session name (defaults to `ferrox` when omitted).
    pub session_name: Option<String>,
    /// External ID, when the trust policy requires one.
    pub external_id: Option<String>,
    /// Session duration in seconds (STS default is 3600 when omitted).
    pub duration_secs: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `providers[].retry` was a dead field, removed in #145. Configs written
    /// against older versions still carry it, so deserialization must keep
    /// ignoring the key rather than failing — i.e. `ProviderConfig` must never
    /// gain `#[serde(deny_unknown_fields)]`.
    #[test]
    fn provider_config_still_loads_when_a_legacy_retry_key_is_present() {
        let yaml = r#"
name: anthropic-primary
type: anthropic
api_key: sk-test
retry:
  max_attempts: 2
circuit_breaker:
  failure_threshold: 3
"#;
        let cfg: ProviderConfig =
            serde_yaml::from_str(yaml).expect("legacy retry key must not break parsing");

        assert_eq!(cfg.name, "anthropic-primary");
        assert_eq!(cfg.provider_type, ProviderType::Anthropic);
        assert_eq!(cfg.api_key.as_deref(), Some("sk-test"));
        // The still-supported sibling override must survive alongside it.
        assert_eq!(
            cfg.circuit_breaker
                .expect("circuit_breaker")
                .failure_threshold,
            3
        );
    }

    /// `defaults.retry` is real and read by the gateway — guard against it
    /// being removed along with the per-provider one.
    #[test]
    fn defaults_config_still_parses_retry() {
        let yaml = "retry:\n  max_attempts: 7\n";
        let cfg: DefaultsConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.retry.max_attempts, 7);
    }
}
