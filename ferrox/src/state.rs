use std::sync::Arc;

use crate::config::Config;
use crate::jwks::JwksCache;
use crate::metrics::Metrics;
use crate::providers::ProviderRegistry;
use crate::ratelimit::RateLimitBackend;
use crate::router::ModelRouter;
use crate::usage_writer::UsageWriter;

#[derive(Clone)]
#[allow(dead_code)] // metrics used in Phase 3 telemetry
pub struct AppState {
    pub config: Arc<Config>,
    pub providers: Arc<ProviderRegistry>,
    pub router: Arc<ModelRouter>,
    /// Pluggable rate limit backend (memory or Redis).
    /// Handles both virtual-key and JWT per-tenant rate limiting.
    pub rate_limit_backend: Arc<dyn RateLimitBackend>,
    pub metrics: Arc<Metrics>,
    pub ready: Arc<std::sync::atomic::AtomicBool>,
    /// JWKS cache for JWT validation. Populated at startup, refreshed in background.
    pub jwks_cache: Arc<JwksCache>,
    /// Async batched writer for persisting per-request token usage to Postgres.
    pub usage_writer: UsageWriter,
}
