use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use crate::config::RateLimitConfig;
use crate::ratelimit::backend::RateLimitBackend;
use crate::ratelimit::token_bucket::TokenBucket;
use crate::telemetry::metrics::{RATELIMIT_ALLOWED_TOTAL, RATELIMIT_DENIED_TOTAL};

/// In-process token bucket rate limiter.
///
/// Each key gets its own `TokenBucket` created on first use. This is the
/// default backend and behaves identically to the pre-Phase-2 implementation.
///
/// This backend is per-instance; under horizontal scaling each replica has
/// independent counters. Use the Redis backend for distributed enforcement.
pub struct MemoryBackend {
    buckets: RwLock<HashMap<String, Arc<TokenBucket>>>,
}

impl MemoryBackend {
    pub fn new() -> Self {
        Self {
            buckets: RwLock::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl RateLimitBackend for MemoryBackend {
    async fn check_and_record(&self, key: &str, limit: &RateLimitConfig) -> Result<(), ()> {
        // Fast path: bucket already exists
        {
            let map = self.buckets.read().unwrap();
            if let Some(bucket) = map.get(key) {
                let allowed = bucket.try_consume();
                if allowed {
                    RATELIMIT_ALLOWED_TOTAL.with_label_values(&["memory"]).inc();
                    return Ok(());
                } else {
                    RATELIMIT_DENIED_TOTAL.with_label_values(&["memory"]).inc();
                    return Err(());
                }
            }
        }

        // Slow path: create bucket on first use
        let mut map = self.buckets.write().unwrap();
        let bucket = map
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(TokenBucket::new(limit.requests_per_minute, limit.burst)));

        let allowed = bucket.try_consume();
        if allowed {
            RATELIMIT_ALLOWED_TOTAL.with_label_values(&["memory"]).inc();
            Ok(())
        } else {
            RATELIMIT_DENIED_TOTAL.with_label_values(&["memory"]).inc();
            Err(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit(rpm: u32, burst: u32) -> RateLimitConfig {
        RateLimitConfig {
            requests_per_minute: rpm,
            burst,
        }
    }

    #[tokio::test]
    async fn allows_up_to_burst() {
        let backend = MemoryBackend::new();
        let lim = limit(60, 3);
        assert!(backend.check_and_record("key", &lim).await.is_ok());
        assert!(backend.check_and_record("key", &lim).await.is_ok());
        assert!(backend.check_and_record("key", &lim).await.is_ok());
        assert!(backend.check_and_record("key", &lim).await.is_err());
    }

    #[tokio::test]
    async fn reuses_bucket_across_calls() {
        let backend = MemoryBackend::new();
        let lim = limit(60, 1);
        assert!(backend.check_and_record("key", &lim).await.is_ok());
        // Bucket exhausted — same bucket is returned on second call
        assert!(backend.check_and_record("key", &lim).await.is_err());
    }

    #[tokio::test]
    async fn isolates_different_keys() {
        let backend = MemoryBackend::new();
        let lim = limit(60, 1);
        // Exhaust key-a
        assert!(backend.check_and_record("key-a", &lim).await.is_ok());
        assert!(backend.check_and_record("key-a", &lim).await.is_err());
        // key-b still has its own fresh bucket
        assert!(backend.check_and_record("key-b", &lim).await.is_ok());
    }

    #[tokio::test]
    async fn zero_rpm_always_denies() {
        let backend = MemoryBackend::new();
        let lim = limit(0, 0);
        assert!(backend.check_and_record("key", &lim).await.is_err());
    }
}
