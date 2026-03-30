use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};

use crate::config::CircuitBreakerConfig;
use crate::telemetry::metrics::{CIRCUIT_BREAKER_STATE, CIRCUIT_BREAKER_TRIPS_TOTAL};

/// Circuit breaker states encoded as u8 for atomic storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    Closed = 0,
    Open = 1,
    HalfOpen = 2,
}

impl CircuitState {
    pub fn as_f64(self) -> f64 {
        self as u8 as f64
    }
}

impl From<u8> for CircuitState {
    fn from(v: u8) -> Self {
        match v {
            0 => CircuitState::Closed,
            1 => CircuitState::Open,
            _ => CircuitState::HalfOpen,
        }
    }
}

/// Lock-free circuit breaker.
///
/// All state transitions use `compare_exchange` — no mutexes.
pub struct CircuitBreaker {
    /// 0 = Closed, 1 = Open, 2 = HalfOpen
    state: AtomicU8,
    /// Consecutive failures while Closed
    failure_count: AtomicU32,
    /// Consecutive successes while HalfOpen
    success_count: AtomicU32,
    /// Unix timestamp (ms) of the last recorded failure
    last_failure_ms: AtomicU64,
    /// CAS guard: ensures only one probe request runs while HalfOpen
    half_open_probe_in_flight: AtomicBool,
    config: CircuitBreakerConfig,
    /// Metric labels (provider name + model alias)
    provider: String,
    model_alias: String,
}

impl CircuitBreaker {
    pub fn new(
        config: CircuitBreakerConfig,
        provider: impl Into<String>,
        model_alias: impl Into<String>,
    ) -> Self {
        let provider = provider.into();
        let model_alias = model_alias.into();
        // Initialise the gauge so it shows up in metrics from startup
        CIRCUIT_BREAKER_STATE
            .with_label_values(&[&provider, &model_alias])
            .set(CircuitState::Closed.as_f64());
        Self {
            state: AtomicU8::new(CircuitState::Closed as u8),
            failure_count: AtomicU32::new(0),
            success_count: AtomicU32::new(0),
            last_failure_ms: AtomicU64::new(0),
            half_open_probe_in_flight: AtomicBool::new(false),
            config,
            provider,
            model_alias,
        }
    }

    /// Returns `true` if a request should be allowed through.
    pub fn is_available(&self) -> bool {
        match self.state() {
            CircuitState::Closed => true,
            CircuitState::Open => {
                let last_failure = self.last_failure_ms.load(Ordering::Acquire);
                let now = now_ms();
                let recovery_ms = self.config.recovery_timeout_secs * 1000;
                if now.saturating_sub(last_failure) >= recovery_ms {
                    // Try to transition Open → HalfOpen
                    if self
                        .state
                        .compare_exchange(
                            CircuitState::Open as u8,
                            CircuitState::HalfOpen as u8,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        self.success_count.store(0, Ordering::Release);
                        self.half_open_probe_in_flight
                            .store(false, Ordering::Release);
                        self.set_state_metric(CircuitState::HalfOpen);
                    }
                    self.try_claim_probe()
                } else {
                    false
                }
            }
            CircuitState::HalfOpen => self.try_claim_probe(),
        }
    }

    fn try_claim_probe(&self) -> bool {
        self.half_open_probe_in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Record a successful upstream call.
    pub fn record_success(&self) {
        match self.state() {
            CircuitState::Closed => {
                self.failure_count.store(0, Ordering::Release);
            }
            CircuitState::HalfOpen => {
                let successes = self.success_count.fetch_add(1, Ordering::AcqRel) + 1;
                if successes >= self.config.success_threshold {
                    let _ = self.state.compare_exchange(
                        CircuitState::HalfOpen as u8,
                        CircuitState::Closed as u8,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    self.failure_count.store(0, Ordering::Release);
                    self.success_count.store(0, Ordering::Release);
                    self.half_open_probe_in_flight
                        .store(false, Ordering::Release);
                    self.set_state_metric(CircuitState::Closed);
                    tracing::info!(
                        provider = %self.provider,
                        model_alias = %self.model_alias,
                        "Circuit breaker closed after successful probe"
                    );
                } else {
                    self.half_open_probe_in_flight
                        .store(false, Ordering::Release);
                }
            }
            CircuitState::Open => {}
        }
    }

    /// Record a failed upstream call.
    pub fn record_failure(&self) {
        self.last_failure_ms.store(now_ms(), Ordering::Release);
        match self.state() {
            CircuitState::Closed => {
                let failures = self.failure_count.fetch_add(1, Ordering::AcqRel) + 1;
                if failures >= self.config.failure_threshold {
                    let _ = self.state.compare_exchange(
                        CircuitState::Closed as u8,
                        CircuitState::Open as u8,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    self.set_state_metric(CircuitState::Open);
                    CIRCUIT_BREAKER_TRIPS_TOTAL
                        .with_label_values(&[&self.provider])
                        .inc();
                    tracing::warn!(
                        provider = %self.provider,
                        model_alias = %self.model_alias,
                        threshold = self.config.failure_threshold,
                        "Circuit breaker opened"
                    );
                }
            }
            CircuitState::HalfOpen => {
                let _ = self.state.compare_exchange(
                    CircuitState::HalfOpen as u8,
                    CircuitState::Open as u8,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                self.half_open_probe_in_flight
                    .store(false, Ordering::Release);
                self.success_count.store(0, Ordering::Release);
                self.set_state_metric(CircuitState::Open);
                CIRCUIT_BREAKER_TRIPS_TOTAL
                    .with_label_values(&[&self.provider])
                    .inc();
                tracing::warn!(
                    provider = %self.provider,
                    model_alias = %self.model_alias,
                    "Circuit breaker re-opened after failed probe"
                );
            }
            CircuitState::Open => {}
        }
    }

    /// Current state — exposed for testing and metrics.
    pub fn state(&self) -> CircuitState {
        CircuitState::from(self.state.load(Ordering::Acquire))
    }

    fn set_state_metric(&self, state: CircuitState) {
        CIRCUIT_BREAKER_STATE
            .with_label_values(&[&self.provider, &self.model_alias])
            .set(state.as_f64());
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cb(threshold: u32, success_threshold: u32, recovery_secs: u64) -> CircuitBreaker {
        CircuitBreaker::new(
            CircuitBreakerConfig {
                failure_threshold: threshold,
                success_threshold,
                recovery_timeout_secs: recovery_secs,
            },
            "test-provider",
            "test-model",
        )
    }

    #[test]
    fn starts_closed_and_allows_requests() {
        let c = cb(3, 2, 30);
        assert_eq!(c.state(), CircuitState::Closed);
        assert!(c.is_available());
    }

    #[test]
    fn opens_after_failure_threshold() {
        let c = cb(3, 2, 30);
        c.record_failure();
        assert_eq!(c.state(), CircuitState::Closed);
        c.record_failure();
        assert_eq!(c.state(), CircuitState::Closed);
        c.record_failure(); // 3rd failure — threshold reached
        assert_eq!(c.state(), CircuitState::Open);
        assert!(!c.is_available());
    }

    #[test]
    fn success_resets_failure_count() {
        let c = cb(3, 2, 30);
        c.record_failure();
        c.record_failure();
        c.record_success(); // resets count
        c.record_failure();
        c.record_failure();
        // Still only 2 consecutive failures after reset — should be Closed
        assert_eq!(c.state(), CircuitState::Closed);
    }

    #[test]
    fn transitions_to_half_open_after_recovery_timeout() {
        let c = cb(1, 1, 0); // 0-second recovery — immediate
        c.record_failure(); // opens the circuit
        assert_eq!(c.state(), CircuitState::Open);

        // Back-date the last_failure_ms so recovery_timeout has elapsed
        c.last_failure_ms.store(0, Ordering::Release);

        // is_available should transition to HalfOpen and grant the probe
        assert!(c.is_available());
        assert_eq!(c.state(), CircuitState::HalfOpen);
    }

    #[test]
    fn closes_after_probe_succeeds() {
        let c = cb(1, 1, 0);
        c.record_failure();
        c.last_failure_ms.store(0, Ordering::Release);
        assert!(c.is_available()); // grants probe → HalfOpen
        c.record_success();
        assert_eq!(c.state(), CircuitState::Closed);
    }

    #[test]
    fn reopens_when_probe_fails() {
        let c = cb(1, 1, 0);
        c.record_failure();
        c.last_failure_ms.store(0, Ordering::Release);
        assert!(c.is_available()); // HalfOpen
        c.record_failure(); // probe failed
        assert_eq!(c.state(), CircuitState::Open);
    }

    #[test]
    fn only_one_probe_allowed_in_half_open() {
        let c = cb(1, 1, 0);
        c.record_failure();
        c.last_failure_ms.store(0, Ordering::Release);

        let first = c.is_available(); // transitions to HalfOpen, grants probe
        let second = c.is_available(); // already HalfOpen, probe in flight → denied

        assert!(first);
        assert!(!second, "Only one probe should be allowed at a time");
    }

    #[test]
    fn closed_when_not_enough_failures() {
        let c = cb(5, 2, 30);
        for _ in 0..4 {
            c.record_failure();
        }
        assert_eq!(c.state(), CircuitState::Closed);
        assert!(c.is_available());
    }

    #[test]
    fn requires_multiple_successes_to_close() {
        let c = cb(1, 3, 0);
        c.record_failure();
        c.last_failure_ms.store(0, Ordering::Release);
        assert!(c.is_available()); // HalfOpen, probe granted

        c.record_success(); // 1/3
                            // Release probe so subsequent calls get in
                            // success_count now 1 but threshold is 3 — still HalfOpen
        assert_eq!(c.state(), CircuitState::HalfOpen);

        // Simulate two more probes succeeding
        // (probe_in_flight was released in record_success when threshold not yet reached)
        c.record_success(); // 2/3
        c.record_success(); // 3/3 — should close
        assert_eq!(c.state(), CircuitState::Closed);
    }
}
