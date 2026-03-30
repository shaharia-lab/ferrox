use std::sync::atomic::{AtomicU64, Ordering};

/// Lock-free token bucket, modeled after nginx's `limit_req_zone`.
///
/// Tokens are stored as **milli-tokens** (×1000) to avoid floating-point
/// arithmetic while maintaining sub-token precision.
///
/// On each request:
/// 1. Read `last_refill_ms` and compute elapsed time
/// 2. Add `elapsed_ms * refill_rate_milli_per_ms` to current tokens
/// 3. CAS: subtract 1000 (= 1 token) if tokens ≥ 1000, else reject
///
/// This is per-pod and approximate — see PRD §5.2.
pub struct TokenBucket {
    /// Current token count × 1000. Stored as u64 to avoid overflow.
    tokens_milli: AtomicU64,
    /// Unix timestamp (ms) of the last refill
    last_refill_ms: AtomicU64,
    /// Bucket capacity × 1000 (= burst * 1000)
    capacity_milli: u64,
    /// Tokens added per millisecond × 1000
    refill_rate_milli_per_ms: u64,
}

impl TokenBucket {
    /// Create a new token bucket.
    ///
    /// - `requests_per_minute`: sustained throughput target
    /// - `burst`: maximum instantaneous burst (token bucket capacity)
    pub fn new(requests_per_minute: u32, burst: u32) -> Self {
        // refill rate: requests_per_minute / 60_000 ms = tokens/ms
        // stored × 1000: requests_per_minute * 1000 / 60_000
        let refill_rate_milli_per_ms = (requests_per_minute as u64 * 1000) / 60_000;
        let capacity_milli = burst as u64 * 1000;

        Self {
            // Start full
            tokens_milli: AtomicU64::new(capacity_milli),
            last_refill_ms: AtomicU64::new(now_ms()),
            capacity_milli,
            refill_rate_milli_per_ms,
        }
    }

    /// Attempt to consume one token.
    ///
    /// Returns `true` if the request is allowed, `false` if rate-limited.
    /// Lock-free: uses a CAS loop to update tokens atomically.
    pub fn try_consume(&self) -> bool {
        let now = now_ms();

        // Load current state
        let last = self.last_refill_ms.load(Ordering::Acquire);
        let elapsed = now.saturating_sub(last);

        // Compute tokens to add
        let add = elapsed.saturating_mul(self.refill_rate_milli_per_ms);

        // Try to atomically update tokens_milli and consume one token.
        // We do a single CAS loop rather than separate read-then-CAS to
        // avoid a TOCTOU window.
        let mut current = self.tokens_milli.load(Ordering::Acquire);
        loop {
            let refilled = current.saturating_add(add).min(self.capacity_milli);
            if refilled < 1000 {
                // Not enough tokens
                return false;
            }
            let new_tokens = refilled - 1000;
            match self.tokens_milli.compare_exchange_weak(
                current,
                new_tokens,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // Successfully consumed; update last_refill_ms (best-effort, not critical)
                    let _ = self.last_refill_ms.compare_exchange(
                        last,
                        now,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    return true;
                }
                Err(actual) => {
                    // Another thread modified tokens; retry with updated value
                    current = actual;
                }
            }
        }
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

    #[test]
    fn allows_up_to_burst() {
        // 60 rpm, burst = 5
        let bucket = TokenBucket::new(60, 5);
        // First 5 should pass, 6th should fail
        for i in 0..5 {
            assert!(bucket.try_consume(), "request {} should pass", i + 1);
        }
        assert!(!bucket.try_consume(), "6th request should be rate limited");
    }

    #[test]
    fn refills_over_time() {
        // 60000 rpm (1000/s = 1/ms), burst = 1
        let bucket = TokenBucket::new(60_000, 1);
        assert!(bucket.try_consume()); // consume the 1 token
        assert!(!bucket.try_consume()); // empty

        // Manually back-date last_refill to simulate 2ms elapsed → 2 tokens added
        let past = now_ms().saturating_sub(2);
        bucket.last_refill_ms.store(past, Ordering::Release);

        assert!(bucket.try_consume()); // refilled
    }

    #[test]
    fn zero_rpm_always_limits() {
        let bucket = TokenBucket::new(0, 0);
        assert!(!bucket.try_consume());
    }
}
