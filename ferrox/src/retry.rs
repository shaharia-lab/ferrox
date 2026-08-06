use std::future::Future;
use std::time::Duration;

use rand::Rng;

use crate::config::RetryConfig;
use crate::error::ProxyError;
use crate::telemetry::metrics::RETRIES_TOTAL;

/// Determines whether a `ProxyError` is transient and should be retried.
pub fn is_retryable(e: &ProxyError) -> bool {
    match e {
        // Transient — retry on same target
        ProxyError::UpstreamTimeout(_) => true,
        ProxyError::CircuitOpen(_) => true,
        ProxyError::ProviderError { status, .. } => *status >= 500 || *status == 429,
        ProxyError::HttpClientError(e) => e.is_timeout() || e.is_connect(),
        ProxyError::StreamError(_) => true,

        // Non-transient — do not retry
        ProxyError::Unauthorized(_) => false,
        ProxyError::Forbidden(_) => false,
        ProxyError::ModelNotFound(_) => false,
        ProxyError::RateLimited(_) => false,
        ProxyError::ConfigError(_) => false,
        ProxyError::SerializationError(_) => false,
        ProxyError::AwsError(_) => false,
        ProxyError::BudgetExceeded(_) => false,
    }
}

/// Determines whether a primary-target failure should degrade to the fallback
/// chain (and count as a circuit-breaker failure).
///
/// This is deliberately broader than [`is_retryable`]: every transient error
/// that is worth retrying on the *same* target is also worth failing over, but
/// in addition an upstream **403** triggers failover here while staying
/// non-retryable above.
///
/// Why 403: some providers report plan/quota exhaustion with 403 rather than
/// 429 (e.g. Moonshot Kimi's `access_terminated_error` when a coding-plan
/// billing cycle is used up). Retrying the *same* provider is pointless — it
/// will keep returning 403 — so [`is_retryable`] leaves it alone and we avoid
/// wasting backoff sleeps on a provider we know is exhausted. But we DO want to
/// serve the request from the other provider, and we want the repeated 403s to
/// trip the circuit breaker so subsequent requests skip the dead provider
/// entirely until it recovers.
///
/// This matches on the UPSTREAM provider's status (`ProviderError`). The
/// gateway's own auth rejection is `ProxyError::Forbidden`, which must NOT
/// trigger failover — a client's bad virtual key should fail closed, not burn
/// the fallback provider's quota.
pub fn should_failover(e: &ProxyError) -> bool {
    is_retryable(e) || matches!(e, ProxyError::ProviderError { status: 403, .. })
}

/// Execute an async operation with retries on the **same target**.
///
/// `provider` and `model_alias` are used for the `ferrox_retries_total` metric.
/// Pass empty strings if the context is not available.
///
/// Returns `Ok(T)` on success, or the last error after all attempts are
/// exhausted. The caller is responsible for trying the next target/fallback
/// if this returns `Err`.
pub async fn execute_with_retry<F, Fut, T>(
    config: &RetryConfig,
    provider: &str,
    model_alias: &str,
    mut f: F,
) -> Result<T, ProxyError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ProxyError>>,
{
    let max_attempts = config.max_attempts.max(1);
    let mut last_error: Option<ProxyError> = None;

    for attempt in 0..max_attempts {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if !is_retryable(&e) {
                    return Err(e);
                }
                if attempt > 0 {
                    RETRIES_TOTAL
                        .with_label_values(&[provider, model_alias])
                        .inc();
                }
                tracing::debug!(
                    attempt = attempt + 1,
                    max_attempts,
                    provider,
                    model_alias,
                    error = %e,
                    "Retryable error — backing off"
                );
                last_error = Some(e);

                if attempt + 1 < max_attempts {
                    let backoff = backoff_duration(attempt, config);
                    tokio::time::sleep(backoff).await;
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| ProxyError::ProviderError {
        provider: String::new(),
        status: 502,
        message: "All retry attempts exhausted".to_string(),
    }))
}

/// Compute the backoff duration for `attempt` (0-indexed).
///
/// Formula: `min(initial_ms * 2^attempt, max_ms)` + optional jitter in `[0, initial_ms)`.
fn backoff_duration(attempt: u32, config: &RetryConfig) -> Duration {
    let shift = attempt.min(20) as u64;
    let multiplier = 1u64.checked_shl(shift as u32).unwrap_or(u64::MAX);
    let base = config.initial_backoff_ms.saturating_mul(multiplier);
    let capped = base.min(config.max_backoff_ms);

    let jitter_ms = if config.jitter {
        rand::thread_rng().gen_range(0..=config.initial_backoff_ms)
    } else {
        0
    };

    Duration::from_millis(capped.saturating_add(jitter_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retry_cfg(max_attempts: u32) -> RetryConfig {
        RetryConfig {
            max_attempts,
            initial_backoff_ms: 0, // no sleep in tests
            max_backoff_ms: 0,
            jitter: false,
        }
    }

    #[tokio::test]
    async fn succeeds_on_first_attempt() {
        let result = execute_with_retry(&retry_cfg(3), "p", "m", || async {
            Ok::<i32, ProxyError>(42)
        })
        .await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn retries_then_succeeds() {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let c = counter.clone();
        let result = execute_with_retry(&retry_cfg(3), "p", "m", move || {
            let count = c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                if count < 2 {
                    Err(ProxyError::UpstreamTimeout("timeout".into()))
                } else {
                    Ok(99i32)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 99);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn does_not_retry_non_retryable() {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let c = counter.clone();
        let result = execute_with_retry(&retry_cfg(3), "p", "m", move || {
            c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { Err::<i32, ProxyError>(ProxyError::Unauthorized("nope".into())) }
        })
        .await;
        assert!(matches!(result, Err(ProxyError::Unauthorized(_))));
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn exhausts_all_attempts() {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let c = counter.clone();
        let result = execute_with_retry(&retry_cfg(3), "p", "m", move || {
            c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { Err::<i32, ProxyError>(ProxyError::UpstreamTimeout("t".into())) }
        })
        .await;
        assert!(matches!(result, Err(ProxyError::UpstreamTimeout(_))));
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    // ── is_retryable ─────────────────────────────────────────────────────────

    #[test]
    fn upstream_timeout_is_retryable() {
        assert!(is_retryable(&ProxyError::UpstreamTimeout("t".into())));
    }

    #[test]
    fn circuit_open_is_retryable() {
        assert!(is_retryable(&ProxyError::CircuitOpen("open".into())));
    }

    #[test]
    fn provider_5xx_is_retryable() {
        assert!(is_retryable(&ProxyError::ProviderError {
            provider: "p".into(),
            status: 503,
            message: "down".into(),
        }));
    }

    #[test]
    fn provider_429_is_retryable() {
        assert!(is_retryable(&ProxyError::ProviderError {
            provider: "p".into(),
            status: 429,
            message: "rate".into(),
        }));
    }

    #[test]
    fn provider_403_is_not_retryable_on_same_target() {
        // Retrying an exhausted provider is pointless — it keeps returning 403.
        assert!(!is_retryable(&ProxyError::ProviderError {
            provider: "p".into(),
            status: 403,
            message: "access_terminated_error".into(),
        }));
    }

    #[test]
    fn provider_4xx_non_retryable_status_is_not_retryable() {
        for status in [400, 404, 422] {
            assert!(!is_retryable(&ProxyError::ProviderError {
                provider: "p".into(),
                status,
                message: "client error".into(),
            }));
        }
    }

    // ── should_failover ────────────────────────────────────────────────────────

    #[test]
    fn provider_403_triggers_failover() {
        // Plan/quota exhaustion reported as 403 (e.g. Kimi's
        // access_terminated_error) must degrade to the fallback provider and
        // count toward the circuit breaker.
        assert!(should_failover(&ProxyError::ProviderError {
            provider: "p".into(),
            status: 403,
            message: "access_terminated_error".into(),
        }));
    }

    #[test]
    fn gateway_forbidden_does_not_trigger_failover() {
        // The gateway's own auth rejection (bad virtual key) must fail closed,
        // never burn the fallback provider's quota.
        assert!(!should_failover(&ProxyError::Forbidden("bad key".into())));
    }

    #[test]
    fn retryable_errors_also_trigger_failover() {
        assert!(should_failover(&ProxyError::UpstreamTimeout("t".into())));
        assert!(should_failover(&ProxyError::ProviderError {
            provider: "p".into(),
            status: 503,
            message: "down".into(),
        }));
        assert!(should_failover(&ProxyError::ProviderError {
            provider: "p".into(),
            status: 429,
            message: "rate".into(),
        }));
    }

    #[test]
    fn provider_4xx_non_403_does_not_trigger_failover() {
        for status in [400, 401, 404, 422] {
            assert!(!should_failover(&ProxyError::ProviderError {
                provider: "p".into(),
                status,
                message: "client error".into(),
            }));
        }
    }

    #[test]
    fn stream_error_is_retryable() {
        assert!(is_retryable(&ProxyError::StreamError("broken".into())));
    }

    #[test]
    fn unauthorized_not_retryable() {
        assert!(!is_retryable(&ProxyError::Unauthorized("u".into())));
    }

    #[test]
    fn forbidden_not_retryable() {
        assert!(!is_retryable(&ProxyError::Forbidden("f".into())));
    }

    #[test]
    fn model_not_found_not_retryable() {
        assert!(!is_retryable(&ProxyError::ModelNotFound("m".into())));
    }

    #[test]
    fn rate_limited_not_retryable() {
        assert!(!is_retryable(&ProxyError::RateLimited("r".into())));
    }

    #[test]
    fn aws_error_not_retryable() {
        assert!(!is_retryable(&ProxyError::AwsError("aws".into())));
    }

    // ── backoff_duration ─────────────────────────────────────────────────────

    #[test]
    fn backoff_caps_at_max() {
        let cfg = RetryConfig {
            max_attempts: 10,
            initial_backoff_ms: 100,
            max_backoff_ms: 500,
            jitter: false,
        };
        // attempt 10: 100 * 2^10 = 102400 → capped at 500
        let d = backoff_duration(10, &cfg);
        assert_eq!(d.as_millis(), 500);
    }

    #[test]
    fn backoff_grows_exponentially() {
        let cfg = RetryConfig {
            max_attempts: 5,
            initial_backoff_ms: 100,
            max_backoff_ms: 100_000,
            jitter: false,
        };
        let d0 = backoff_duration(0, &cfg); // 100 * 1 = 100ms
        let d1 = backoff_duration(1, &cfg); // 100 * 2 = 200ms
        let d2 = backoff_duration(2, &cfg); // 100 * 4 = 400ms
        assert_eq!(d0.as_millis(), 100);
        assert_eq!(d1.as_millis(), 200);
        assert_eq!(d2.as_millis(), 400);
    }
}
