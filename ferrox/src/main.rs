mod anthropic_types;
mod auth;
mod budget_enforcer;
mod config;
mod error;
mod handlers;
mod jwks;
mod lb;
mod metrics;
mod providers;
mod ratelimit;
mod retry;
mod router;
mod server;
mod state;
mod telemetry;
mod types;
mod usage_writer;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use clap::Parser;
use metrics::Metrics;
use ratelimit::{MemoryBackend, RateLimitBackend, RedisBackend};
use router::ModelRouter;
use state::AppState;

const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("GIT_SHA"),
    " ",
    env!("BUILD_DATE"),
    ")"
);

/// Ferrox — high-performance stateless LLM API gateway
#[derive(Parser)]
#[command(name = "ferrox", version = VERSION, about, long_about = None)]
struct Cli {
    /// Path to the configuration file
    #[arg(
        short,
        long,
        env = "LLM_PROXY_CONFIG",
        default_value = "config/local.yaml"
    )]
    config: String,
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    // 1. Load .env (local dev only, no-op in prod)
    dotenvy::dotenv().ok();

    // 2. Parse CLI args (handles --version and --help automatically)
    let cli = Cli::parse();

    // 3. Load and validate config
    let config = config::load_config_from(&cli.config)?;

    // 4. Init logging (before anything else that might log)
    telemetry::init_logging(&config.telemetry)?;

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        git_sha = env!("GIT_SHA"),
        build_date = env!("BUILD_DATE"),
        "Starting Ferrox"
    );

    // 5. Build provider registry
    let providers = providers::build_registry(&config.providers, &config.defaults).await?;
    tracing::info!(count = providers.len(), "Providers registered");

    // 6. Build model router (RoutePool per alias, circuit breakers initialized)
    let model_router = ModelRouter::from_config(&config, &providers)?;

    // 7. Build rate limit backend (memory or Redis)
    let rate_limit_backend: Arc<dyn RateLimitBackend> = match &config.rate_limiting.backend {
        config::RateLimitBackendType::Memory => {
            tracing::info!("Rate limit backend: memory");
            Arc::new(MemoryBackend::new())
        }
        config::RateLimitBackendType::Redis => {
            let url = config
                .rate_limiting
                .redis_url
                .as_deref()
                .expect("redis_url is required when backend is redis");
            let backend = RedisBackend::new(
                url,
                config.rate_limiting.redis_pool_size,
                config.rate_limiting.redis_key_prefix.clone(),
                config.rate_limiting.redis_fail_open,
            )?;
            tracing::info!(url = %url, "Rate limit backend: redis");
            Arc::new(backend)
        }
    };

    // 8. Build JWKS cache and prefetch
    let http_client = reqwest::Client::new();
    let jwks_cache = Arc::new(jwks::JwksCache::new(
        config.trusted_issuers.clone(),
        config.jwks_cache_ttl_secs,
        http_client,
    ));
    jwks_cache.prefetch_all().await;
    if !config.trusted_issuers.is_empty() {
        jwks_cache.clone().spawn_refresh_task();
        tracing::info!(
            count = config.trusted_issuers.len(),
            "Trusted JWKS issuers configured"
        );
    }

    // 9. Usage writer (optional — requires usage_database_url)
    let usage_writer = if let Some(ref db_url) = config.usage_database_url {
        let pool = sqlx::PgPool::connect(db_url)
            .await
            .map_err(|e| anyhow::anyhow!("failed to connect to usage database: {}", e))?;
        tracing::info!("Usage writer: connected to database");
        usage_writer::spawn_writer(
            pool,
            100,                               // batch_size
            std::time::Duration::from_secs(5), // flush_interval
            10_000,                            // buffer_capacity
        )
    } else {
        tracing::info!("Usage writer: disabled (no usage_database_url configured)");
        usage_writer::noop_writer()
    };

    // 10. Budget enforcer (reuses Redis pool if rate limiting is Redis-backed)
    let budget_enforcer: Arc<dyn budget_enforcer::BudgetEnforcer> =
        match &config.rate_limiting.backend {
            config::RateLimitBackendType::Redis => {
                let url = config.rate_limiting.redis_url.as_deref().unwrap();
                let mut cfg = deadpool_redis::Config::from_url(url);
                cfg.pool = Some(deadpool_redis::PoolConfig::new(
                    config.rate_limiting.redis_pool_size,
                ));
                let pool = cfg
                    .create_pool(Some(deadpool_redis::Runtime::Tokio1))
                    .map_err(|e| anyhow::anyhow!("Budget enforcer Redis pool: {e}"))?;
                tracing::info!("Budget enforcer: Redis");
                Arc::new(budget_enforcer::RedisBudgetEnforcer::new(
                    pool,
                    config.rate_limiting.redis_key_prefix.clone(),
                    config.rate_limiting.redis_fail_open,
                ))
            }
            config::RateLimitBackendType::Memory => {
                tracing::info!("Budget enforcer: disabled (no Redis backend)");
                Arc::new(budget_enforcer::NoopBudgetEnforcer)
            }
        };

    // 11. Init metrics
    let metrics = Metrics::new();

    // 12. Build AppState
    let ready = Arc::new(AtomicBool::new(false));
    let state = AppState {
        config: Arc::new(config),
        providers: Arc::new(providers),
        router: Arc::new(model_router),
        rate_limit_backend,
        metrics: Arc::new(metrics),
        ready: ready.clone(),
        jwks_cache,
        usage_writer,
        budget_enforcer,
    };

    // 11. Build axum router
    let app = server::build_router(state.clone());

    // 12. Bind listener
    let addr = format!("{}:{}", state.config.server.host, state.config.server.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(addr = %addr, "Ferrox listening");

    // 13. Mark ready
    ready.store(true, Ordering::Release);

    // 14. Serve with graceful shutdown
    let graceful_timeout = state.config.server.graceful_shutdown_timeout_secs;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(ready, graceful_timeout))
        .await?;

    // Flush any pending OTEL spans before exit
    telemetry::otel::shutdown();

    tracing::info!("Ferrox shut down cleanly");
    Ok(())
}

async fn shutdown_signal(ready: Arc<AtomicBool>, timeout_secs: u64) {
    use tokio::signal;

    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("Shutdown signal received — draining requests");
    ready.store(false, Ordering::Release);

    tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)).await;
}
