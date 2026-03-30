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
