use std::sync::Arc;

use crate::config::CpConfig;

/// Shared state injected into every control-plane request handler.
#[derive(Clone)]
pub struct CpState {
    /// PostgreSQL connection pool.
    pub db: sqlx::PgPool,
    /// Control-plane configuration.
    pub config: Arc<CpConfig>,
}
