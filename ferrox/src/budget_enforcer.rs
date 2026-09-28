use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use deadpool_redis::Pool;

/// Pre-request budget reservation + post-response reconciliation.
///
/// Uses Redis as a fast distributed counter. The key pattern is
/// `ferrox:budget:{client_id}:{period}` with a TTL matching the budget period.
///
/// The reserve-then-reconcile pattern avoids the TOCTOU race:
/// 1. `reserve_tokens`: atomically checks budget AND increments the counter
///    by a pessimistic estimate (request's `max_tokens`).
/// 2. `reconcile_tokens`: after the response, adjusts the counter to reflect
///    actual usage (refunds unused reserved tokens).
///
/// On Redis unavailability, falls back to allowing the request (fail-open),
/// relying on Phase 3's periodic soft enforcement as a safety net.
#[async_trait]
pub trait BudgetEnforcer: Send + Sync {
    /// Atomically check budget and reserve `estimated_tokens`.
    /// Returns `Ok(())` if the reservation succeeds (within budget),
    /// `Err(())` if the budget would be exceeded.
    async fn reserve_tokens(
        &self,
        client_id: &str,
        period: &str,
        budget: i64,
        estimated_tokens: u32,
    ) -> Result<(), ()>;

    /// Reconcile the reservation with actual token usage.
    /// If `actual < reserved`, refunds the difference.
    /// If `actual > reserved`, increments by the difference.
    async fn reconcile_tokens(&self, client_id: &str, period: &str, reserved: u32, actual: u32);
}

/// No-op enforcer used when Redis is not configured.
/// Always allows requests — budget enforcement falls back to Phase 3 periodic checks.
pub struct NoopBudgetEnforcer;

#[async_trait]
impl BudgetEnforcer for NoopBudgetEnforcer {
    async fn reserve_tokens(
        &self,
        _client_id: &str,
        _period: &str,
        _budget: i64,
        _estimated_tokens: u32,
    ) -> Result<(), ()> {
        Ok(())
    }

    async fn reconcile_tokens(
        &self,
        _client_id: &str,
        _period: &str,
        _reserved: u32,
        _actual: u32,
    ) {
    }
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

/// Lua script for atomic budget check + reservation.
/// Checks if current + reserve <= budget; if so, increments and returns 1.
/// Also sets TTL on first write.  Returns 0 if budget would be exceeded.
const RESERVE_LUA: &str = r#"
local key     = KEYS[1]
local reserve = tonumber(ARGV[1])
local budget  = tonumber(ARGV[2])
local ttl     = tonumber(ARGV[3])

local current = tonumber(redis.call('GET', key) or "0")
if current + reserve > budget then
    return 0
end
redis.call('INCRBY', key, reserve)
if redis.call('TTL', key) == -1 then
    redis.call('EXPIRE', key, ttl)
end
return 1
"#;

/// Lua script for reconciliation: adjusts the counter by a delta.
/// delta > 0 means actual > reserved (under-estimated — add more).
/// delta < 0 means actual < reserved (over-estimated — refund).
const RECONCILE_LUA: &str = r#"
local key   = KEYS[1]
local delta = tonumber(ARGV[1])

if delta == 0 then return 0 end
local new_val = redis.call('INCRBY', key, delta)
-- Prevent counter from going negative (safety guard)
if new_val < 0 then
    redis.call('SET', key, 0)
    new_val = 0
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
    async fn reserve_tokens(
        &self,
        client_id: &str,
        period: &str,
        budget: i64,
        estimated_tokens: u32,
    ) -> Result<(), ()> {
        let redis_key = format!("{}budget:{}:{}", self.key_prefix, client_id, period);
        let ttl = period_ttl_secs(period);

        let mut conn = match self.pool.get().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, client_id = %client_id, "Redis unavailable for budget reservation");
                return if self.fail_open { Ok(()) } else { Err(()) };
            }
        };

        let script = redis::Script::new(RESERVE_LUA);
        let result: redis::RedisResult<i64> = script
            .key(&redis_key)
            .arg(estimated_tokens as i64)
            .arg(budget)
            .arg(ttl)
            .invoke_async(&mut conn)
            .await;

        match result {
            Ok(1) => Ok(()),
            Ok(_) => {
                tracing::info!(
                    client_id = %client_id,
                    "budget exhausted — blocking request"
                );
                Err(())
            }
            Err(e) => {
                tracing::warn!(error = %e, client_id = %client_id, "Redis script error in budget reservation");
                if self.fail_open {
                    Ok(())
                } else {
                    Err(())
                }
            }
        }
    }

    async fn reconcile_tokens(&self, client_id: &str, period: &str, reserved: u32, actual: u32) {
        let delta = actual as i64 - reserved as i64;
        if delta == 0 {
            return;
        }

        let redis_key = format!("{}budget:{}:{}", self.key_prefix, client_id, period);

        let mut conn = match self.pool.get().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, client_id = %client_id, "Redis unavailable for budget reconciliation");
                return;
            }
        };

        let script = redis::Script::new(RECONCILE_LUA);
        let result: redis::RedisResult<i64> = script
            .key(&redis_key)
            .arg(delta)
            .invoke_async(&mut conn)
            .await;

        match result {
            Ok(new_total) => {
                tracing::debug!(
                    client_id = %client_id,
                    reserved = reserved,
                    actual = actual,
                    delta = delta,
                    new_total = new_total,
                    "reconciled budget counter"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, client_id = %client_id, "failed to reconcile budget counter");
            }
        }
    }
}

/// Default reservation estimate when `max_tokens` is not provided in the request.
pub const DEFAULT_RESERVE_TOKENS: u32 = 4096;

/// A pre-request budget reservation that refunds itself unless claimed.
///
/// `auth_middleware` creates one per reserved request and carries it as a
/// request extension. A `RequestFinalizer` claims it once the request has been
/// dispatched and reconciles it against actual usage. Every other exit — a
/// 400/403/404, a dispatch failure, a body the extractor rejects, a handler
/// future dropped on client disconnect, a route that never dispatches — drops
/// the last clone unclaimed, and the drop refunds the whole reservation
/// (`reconcile_tokens(reserved, 0)`), spawned on the runtime since `drop`
/// cannot await.
///
/// Cloning is a refcount bump (request extensions must be `Clone`); the
/// claim is a single atomic swap, so exactly one clone ever settles it. Only
/// requests with a budget pay for the allocation.
#[derive(Clone)]
pub struct BudgetReservation(Arc<ReservationInner>);

struct ReservationInner {
    enforcer: Arc<dyn BudgetEnforcer>,
    client_id: String,
    period: String,
    reserved: u32,
    armed: AtomicBool,
}

/// A claimed reservation: the caller now owes the reconciliation.
pub struct ClaimedReservation {
    pub enforcer: Arc<dyn BudgetEnforcer>,
    pub client_id: String,
    pub period: String,
    pub reserved: u32,
}

impl BudgetReservation {
    pub fn new(
        enforcer: Arc<dyn BudgetEnforcer>,
        client_id: String,
        period: String,
        reserved: u32,
    ) -> Self {
        Self(Arc::new(ReservationInner {
            enforcer,
            client_id,
            period,
            reserved,
            armed: AtomicBool::new(true),
        }))
    }

    /// Tokens this reservation holds against the budget.
    pub fn reserved(&self) -> u32 {
        self.0.reserved
    }

    /// Take over settling the reservation. Returns `Some` exactly once across
    /// all clones; after that, dropping the reservation refunds nothing.
    pub fn claim(&self) -> Option<ClaimedReservation> {
        let inner = &self.0;
        inner
            .armed
            .swap(false, Ordering::AcqRel)
            .then(|| ClaimedReservation {
                enforcer: inner.enforcer.clone(),
                client_id: inner.client_id.clone(),
                period: inner.period.clone(),
                reserved: inner.reserved,
            })
    }
}

impl Drop for ReservationInner {
    fn drop(&mut self) {
        if !*self.armed.get_mut() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let enforcer = self.enforcer.clone();
        let client_id = std::mem::take(&mut self.client_id);
        let period = std::mem::take(&mut self.period);
        let reserved = self.reserved;
        handle.spawn(async move {
            enforcer
                .reconcile_tokens(&client_id, &period, reserved, 0)
                .await;
        });
    }
}

/// Test double that records every `reconcile_tokens` call.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct RecordingBudget {
    pub(crate) reserves: std::sync::atomic::AtomicUsize,
    pub(crate) calls: std::sync::Mutex<Vec<(String, String, u32, u32)>>,
}

#[cfg(test)]
impl RecordingBudget {
    pub(crate) fn reconciles(&self) -> Vec<(String, String, u32, u32)> {
        self.calls.lock().unwrap().clone()
    }
}

#[cfg(test)]
#[async_trait]
impl BudgetEnforcer for RecordingBudget {
    async fn reserve_tokens(&self, _: &str, _: &str, _: i64, _: u32) -> Result<(), ()> {
        self.reserves.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    async fn reconcile_tokens(&self, client_id: &str, period: &str, reserved: u32, actual: u32) {
        self.calls.lock().unwrap().push((
            client_id.to_string(),
            period.to_string(),
            reserved,
            actual,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_enforcer_always_allows() {
        let enforcer = NoopBudgetEnforcer;
        assert!(enforcer
            .reserve_tokens("client-1", "daily", 1000, 500)
            .await
            .is_ok());
        // reconcile should not panic
        enforcer
            .reconcile_tokens("client-1", "daily", 500, 300)
            .await;
    }

    fn reservation(budget: &Arc<RecordingBudget>) -> BudgetReservation {
        BudgetReservation::new(budget.clone(), "c1".into(), "daily".into(), 4096)
    }

    #[tokio::test]
    async fn unclaimed_reservation_refunds_once_on_last_drop() {
        let budget = Arc::new(RecordingBudget::default());
        let r = reservation(&budget);
        let clone = r.clone();
        drop(r);
        tokio::task::yield_now().await;
        assert!(budget.reconciles().is_empty(), "a clone is still alive");

        drop(clone);
        tokio::task::yield_now().await;
        assert_eq!(
            budget.reconciles(),
            vec![("c1".to_string(), "daily".to_string(), 4096, 0)]
        );
    }

    #[tokio::test]
    async fn claimed_reservation_does_not_refund() {
        let budget = Arc::new(RecordingBudget::default());
        let r = reservation(&budget);
        let claimed = r.clone().claim().expect("first claim");
        assert_eq!(
            (claimed.client_id.as_str(), claimed.period.as_str()),
            ("c1", "daily")
        );
        assert_eq!(claimed.reserved, 4096);
        assert!(r.claim().is_none(), "claimed at most once across clones");

        drop(r);
        tokio::task::yield_now().await;
        assert!(budget.reconciles().is_empty());
    }

    #[test]
    fn drop_outside_a_runtime_does_not_panic() {
        let budget = Arc::new(RecordingBudget::default());
        drop(reservation(&budget));
        assert!(budget.reconciles().is_empty());
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
