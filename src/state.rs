use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::config::Config;
use crate::jwks::JwksCache;
use crate::metrics::Metrics;
use crate::providers::ProviderRegistry;
use crate::ratelimit::token_bucket::TokenBucket;
use crate::ratelimit::RateLimiter;
use crate::router::ModelRouter;

#[derive(Clone)]
#[allow(dead_code)] // metrics used in Phase 3 telemetry
pub struct AppState {
    pub config: Arc<Config>,
    pub providers: Arc<ProviderRegistry>,
    pub router: Arc<ModelRouter>,
    pub rate_limiter: Arc<RateLimiter>,
    pub metrics: Arc<Metrics>,
    pub ready: Arc<std::sync::atomic::AtomicBool>,
    /// JWKS cache for JWT validation. Populated at startup, refreshed in background.
    pub jwks_cache: Arc<JwksCache>,
    /// Per-tenant in-process token buckets for JWT-authenticated requests.
    pub jwt_rate_limiters: Arc<RwLock<HashMap<String, Arc<TokenBucket>>>>,
}
