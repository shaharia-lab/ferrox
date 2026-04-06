use async_trait::async_trait;
use deadpool_redis::Pool;

/// Pre-request budget check + post-response token recording.
///
/// Uses Redis as a fast distributed counter. The key pattern is
/// `ferrox:budget:{client_id}:{period}` with a TTL matching the budget period.
///
/// On Redis unavailability, falls back to allowing the request (fail-open),
/// relying on Phase 3's periodic soft enforcement as a safety net.
#[async_trait]
pub trait BudgetEnforcer: Send + Sync {
    /// Check if the client is within budget.  Returns `Ok(())` if allowed,
    /// `Err(())` if the budget is exhausted.
    async fn check_budget(&self, client_id: &str, period: &str, budget: i64) -> Result<(), ()>;

    /// Record actual tokens consumed after a response completes.
    /// Atomically increments the Redis counter.
    async fn record_tokens(&self, client_id: &str, period: &str, tokens: u32);
}

/// No-op enforcer used when Redis is not configured.
/// Always allows requests — budget enforcement falls back to Phase 3 periodic checks.
pub struct NoopBudgetEnforcer;

#[async_trait]
impl BudgetEnforcer for NoopBudgetEnforcer {
    async fn check_budget(&self, _client_id: &str, _period: &str, _budget: i64) -> Result<(), ()> {
        Ok(())
    }

    async fn record_tokens(&self, _client_id: &str, _period: &str, _tokens: u32) {}
}

/// Redis-backed budget enforcer.
pub struct RedisBudgetEnforcer {
    pool: Pool,
    key_prefix: String,
    fail_open: bool,
}

impl RedisBudgetEnforcer {
    pub fn new(pool: Pool, key_prefix: String, fail_open: bool) -> Self {
        Self {
            pool,
            key_prefix,
            fail_open,
        }
    }
}

/// Lua script for atomic budget check.
/// Returns 1 if within budget, 0 if exceeded.
const BUDGET_CHECK_LUA: &str = r#"
local key    = KEYS[1]
local budget = tonumber(ARGV[1])

local current = tonumber(redis.call('GET', key) or "0")
if current < budget then
    return 1
else
    return 0
end
"#;

/// Lua script for atomic token recording with TTL management.
/// Increments the counter by the given amount and sets TTL if the key is new.
const RECORD_TOKENS_LUA: &str = r#"
local key    = KEYS[1]
local tokens = tonumber(ARGV[1])
local ttl    = tonumber(ARGV[2])

local new_val = redis.call('INCRBY', key, tokens)
-- Set TTL only if the key doesn't have one yet (first write in this period)
if redis.call('TTL', key) == -1 then
    redis.call('EXPIRE', key, ttl)
end
return new_val
"#;

fn period_ttl_secs(period: &str) -> i64 {
    match period {
        "daily" => 86400,
        "monthly" => 86400 * 31, // conservative upper bound
        _ => 86400,
    }
}

#[async_trait]
impl BudgetEnforcer for RedisBudgetEnforcer {
    async fn check_budget(&self, client_id: &str, period: &str, budget: i64) -> Result<(), ()> {
        let redis_key = format!("{}budget:{}:{}", self.key_prefix, client_id, period);

        let mut conn = match self.pool.get().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, client_id = %client_id, "Redis unavailable for budget check");
                return if self.fail_open { Ok(()) } else { Err(()) };
            }
        };

        let script = redis::Script::new(BUDGET_CHECK_LUA);
        let result: redis::RedisResult<i64> = script
            .key(&redis_key)
            .arg(budget)
            .invoke_async(&mut conn)
            .await;

        match result {
            Ok(1) => Ok(()),
            Ok(_) => {
                tracing::info!(client_id = %client_id, "budget exhausted — blocking request");
                Err(())
            }
            Err(e) => {
                tracing::warn!(error = %e, client_id = %client_id, "Redis script error in budget check");
                if self.fail_open {
                    Ok(())
                } else {
                    Err(())
                }
            }
        }
    }

    async fn record_tokens(&self, client_id: &str, period: &str, tokens: u32) {
        if tokens == 0 {
            return;
        }

        let redis_key = format!("{}budget:{}:{}", self.key_prefix, client_id, period);
        let ttl = period_ttl_secs(period);

        let mut conn = match self.pool.get().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, client_id = %client_id, "Redis unavailable for token recording");
                return;
            }
        };

        let script = redis::Script::new(RECORD_TOKENS_LUA);
        let result: redis::RedisResult<i64> = script
            .key(&redis_key)
            .arg(tokens as i64)
            .arg(ttl)
            .invoke_async(&mut conn)
            .await;

        match result {
            Ok(new_total) => {
                tracing::debug!(
                    client_id = %client_id,
                    tokens = tokens,
                    new_total = new_total,
                    "recorded tokens in budget counter"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, client_id = %client_id, "failed to record tokens in Redis");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_enforcer_always_allows() {
        let enforcer = NoopBudgetEnforcer;
        assert!(enforcer
            .check_budget("client-1", "daily", 1000)
            .await
            .is_ok());
        // record_tokens should not panic
        enforcer.record_tokens("client-1", "daily", 500).await;
    }

    #[tokio::test]
    async fn period_ttl_daily() {
        assert_eq!(period_ttl_secs("daily"), 86400);
    }

    #[tokio::test]
    async fn period_ttl_monthly() {
        assert_eq!(period_ttl_secs("monthly"), 86400 * 31);
    }
}
