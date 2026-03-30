use async_trait::async_trait;
use deadpool_redis::{Config as PoolConfig, Pool, Runtime};

use crate::config::RateLimitConfig;
use crate::ratelimit::backend::RateLimitBackend;
use crate::telemetry::metrics::{
    RATELIMIT_ALLOWED_TOTAL, RATELIMIT_BACKEND_ERRORS_TOTAL, RATELIMIT_DENIED_TOTAL,
};

/// Sliding-window rate limiter backed by Redis.
///
/// Uses a Lua script (single round-trip) to enforce limits atomically across
/// all gateway replicas sharing the same Redis instance.
///
/// On Redis unavailability, the backend either fails open (allows the request
/// with a warning log) or fails closed, controlled by `fail_open`.
pub struct RedisBackend {
    pool: Pool,
    key_prefix: String,
    fail_open: bool,
}

impl RedisBackend {
    pub fn new(
        redis_url: &str,
        pool_size: usize,
        key_prefix: String,
        fail_open: bool,
    ) -> Result<Self, anyhow::Error> {
        let mut cfg = PoolConfig::from_url(redis_url);
        cfg.pool = Some(deadpool_redis::PoolConfig::new(pool_size));
        let pool = cfg
            .create_pool(Some(Runtime::Tokio1))
            .map_err(|e| anyhow::anyhow!("Failed to create Redis pool: {e}"))?;
        Ok(Self {
            pool,
            key_prefix,
            fail_open,
        })
    }
}

/// Atomic sliding-window counter using a sorted set.
///
/// Each request adds an entry with the current timestamp as both score and
/// member. Entries older than the window are pruned before counting.
/// One Lua round-trip per request — no TOCTOU race.
const SLIDING_WINDOW_LUA: &str = r#"
local key    = KEYS[1]
local window = tonumber(ARGV[1])  -- window size in seconds
local limit  = tonumber(ARGV[2])  -- max requests per window
local now    = tonumber(ARGV[3])  -- current unix timestamp (ms)

redis.call('ZREMRANGEBYSCORE', key, 0, now - window * 1000)
local count = redis.call('ZCARD', key)
if count < limit then
  redis.call('ZADD', key, now, now)
  redis.call('PEXPIRE', key, window * 1000)
  return 1
else
  return 0
end
"#;

#[async_trait]
impl RateLimitBackend for RedisBackend {
    async fn check_and_record(&self, key: &str, limit: &RateLimitConfig) -> Result<(), ()> {
        let redis_key = format!("{}{}:60", self.key_prefix, key);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        let mut conn = match self.pool.get().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, key = %key, "Redis connection failed — rate limit backend unavailable");
                RATELIMIT_BACKEND_ERRORS_TOTAL
                    .with_label_values(&["redis"])
                    .inc();
                if self.fail_open {
                    RATELIMIT_ALLOWED_TOTAL.with_label_values(&["redis"]).inc();
                    return Ok(());
                }
                RATELIMIT_DENIED_TOTAL.with_label_values(&["redis"]).inc();
                return Err(());
            }
        };

        let script = redis::Script::new(SLIDING_WINDOW_LUA);
        let result: redis::RedisResult<i64> = script
            .key(&redis_key)
            .arg(60i64) // 60-second window
            .arg(limit.requests_per_minute as i64)
            .arg(now_ms)
            .invoke_async(&mut conn)
            .await;

        match result {
            Ok(1) => {
                RATELIMIT_ALLOWED_TOTAL.with_label_values(&["redis"]).inc();
                Ok(())
            }
            Ok(_) => {
                RATELIMIT_DENIED_TOTAL.with_label_values(&["redis"]).inc();
                Err(())
            }
            Err(e) => {
                tracing::warn!(error = %e, key = %key, "Redis script error — rate limit backend unavailable");
                RATELIMIT_BACKEND_ERRORS_TOTAL
                    .with_label_values(&["redis"])
                    .inc();
                if self.fail_open {
                    RATELIMIT_ALLOWED_TOTAL.with_label_values(&["redis"]).inc();
                    Ok(())
                } else {
                    RATELIMIT_DENIED_TOTAL.with_label_values(&["redis"]).inc();
                    Err(())
                }
            }
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

    fn backend_with_bad_url(fail_open: bool) -> RedisBackend {
        // Use an unreachable URL — pool creation succeeds (lazy), but get() will fail.
        RedisBackend::new(
            "redis://127.0.0.1:1", // nothing listening on port 1
            1,
            "ferrox:rl:test:".to_string(),
            fail_open,
        )
        .expect("pool creation should not fail eagerly")
    }

    #[tokio::test]
    async fn fail_open_allows_request_when_redis_unreachable() {
        let backend = backend_with_bad_url(true);
        // Connection will fail; fail_open=true → Ok(())
        let result = backend.check_and_record("tenant-a", &limit(60, 10)).await;
        assert!(
            result.is_ok(),
            "fail_open should allow request on connection error"
        );
    }

    #[tokio::test]
    async fn fail_closed_denies_request_when_redis_unreachable() {
        let backend = backend_with_bad_url(false);
        // Connection will fail; fail_open=false → Err(())
        let result = backend.check_and_record("tenant-a", &limit(60, 10)).await;
        assert!(
            result.is_err(),
            "fail_closed should deny request on connection error"
        );
    }

    #[tokio::test]
    async fn fail_open_isolates_different_keys() {
        let backend = backend_with_bad_url(true);
        // Both keys should be independently allowed (fail_open)
        assert!(backend
            .check_and_record("key-a", &limit(60, 1))
            .await
            .is_ok());
        assert!(backend
            .check_and_record("key-b", &limit(60, 1))
            .await
            .is_ok());
    }
}
