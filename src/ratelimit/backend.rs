use async_trait::async_trait;

use crate::config::RateLimitConfig;

/// Pluggable rate limit backend.
///
/// Both `memory` and `redis` backends implement this trait. The rest of the
/// gateway is unaware of which backend is active.
#[async_trait]
pub trait RateLimitBackend: Send + Sync {
    /// Returns `Ok(())` if the request is allowed, `Err(())` if rate-limited.
    async fn check_and_record(&self, key: &str, limit: &RateLimitConfig) -> Result<(), ()>;
}
