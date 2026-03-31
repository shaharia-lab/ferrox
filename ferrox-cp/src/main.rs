// ferrox-cp: control plane for the Ferrox LLM gateway
// Phase 3 — public API implemented.
#![allow(dead_code)]
mod config;
mod crypto;
mod db;
mod error;
mod handlers;
mod state;

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use tokio::net::TcpListener;
use tracing::info;

use config::CpConfig;
use crypto::encrypt::encrypt_private_key;
use crypto::keys::generate_keypair;
use error::CpError;
use handlers::{health::health_handler, jwks::jwks_handler, token::token_handler};
use state::CpState;

/// Migrations bundled into the binary at compile time.
/// Also re-used by integration tests via `#[sqlx::test(migrator = "crate::MIGRATOR")]`.
// sqlx::migrate! requires a path with a parent component.
// "./migrations" resolves relative to CARGO_MANIFEST_DIR (crate root).
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialise structured logging.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = CpConfig::from_env().map_err(|e| anyhow::anyhow!("{}", e))?;
    let encryption_key = parse_encryption_key(&config.cp_encryption_key)?;
    let config = Arc::new(config);

    // Connect to Postgres and run pending migrations.
    let db = sqlx::PgPool::connect(&config.database_url)
        .await
        .map_err(|e| anyhow::anyhow!("failed to connect to database: {}", e))?;

    MIGRATOR
        .run(&db)
        .await
        .map_err(|e| anyhow::anyhow!("migration failed: {}", e))?;

    info!("database migrations applied");

    // Ensure at least one active signing key exists.
    seed_signing_key(&db, &encryption_key).await?;

    let state = CpState {
        db,
        config: config.clone(),
    };

    let app = Router::new()
        .route("/.well-known/jwks.json", get(jwks_handler))
        .route("/token", post(token_handler))
        .route("/healthz", get(health_handler))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", config.port);
    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind {}: {}", addr, e))?;

    info!(addr = %addr, "ferrox-cp listening");
    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("server error: {}", e))
}

/// Parse the 64 hex-character `CP_ENCRYPTION_KEY` into a 32-byte array.
pub fn parse_encryption_key(hex_key: &str) -> Result<[u8; 32], CpError> {
    let bytes = hex::decode(hex_key)
        .map_err(|e| CpError::Config(format!("CP_ENCRYPTION_KEY is not valid hex: {e}")))?;
    bytes.try_into().map_err(|_| {
        CpError::Config("CP_ENCRYPTION_KEY must decode to exactly 32 bytes".to_string())
    })
}

/// Stable advisory lock key for the key-seed critical section.
/// Any fixed non-zero i64 works; this one is the fnv-1a hash of "ferrox-cp-keyseed".
const KEY_SEED_ADVISORY_LOCK: i64 = 0x6665_7272_6f78_2d63_i64.wrapping_add(1);

/// If the `signing_keys` table is empty, generate an RSA-2048 keypair, encrypt
/// the private key, and persist it.  Idempotent: does nothing if a key exists.
///
/// Uses a Postgres transaction-scoped advisory lock so concurrent instances
/// cannot both observe an empty table and insert duplicate seed keys (TOCTOU).
/// The lock is released automatically when the transaction commits or rolls back.
async fn seed_signing_key(db: &sqlx::PgPool, encryption_key: &[u8; 32]) -> Result<(), CpError> {
    let mut tx = db.begin().await?;

    // Acquire a transaction-scoped exclusive advisory lock.  Only one instance
    // can hold this lock at a time; others block until the transaction ends.
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(KEY_SEED_ADVISORY_LOCK)
        .execute(&mut *tx)
        .await?;

    // Re-check inside the lock: a concurrent instance may have already seeded.
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM signing_keys WHERE active = true")
        .fetch_one(&mut *tx)
        .await?;

    if count.0 > 0 {
        info!(
            count = count.0,
            "signing keys already present, skipping seed"
        );
        tx.commit().await?;
        return Ok(());
    }

    info!("no signing keys found, generating initial RSA-2048 keypair");

    let kp = generate_keypair()?;
    let encrypted_private_key = encrypt_private_key(&kp.private_key_der, encryption_key);

    sqlx::query("INSERT INTO signing_keys (kid, private_key, public_key) VALUES ($1, $2, $3)")
        .bind(&kp.kid)
        .bind(&encrypted_private_key)
        .bind(&kp.public_key_der)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    info!(kid = %kp.kid, "generated initial signing key");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use db::signing_key_repo::SigningKeyRepository;

    #[test]
    fn parse_encryption_key_valid_hex() {
        let hex = "a".repeat(64);
        let key = parse_encryption_key(&hex).expect("should succeed");
        assert_eq!(key.len(), 32);
        assert!(key.iter().all(|&b| b == 0xaa));
    }

    #[test]
    fn parse_encryption_key_invalid_hex() {
        let result = parse_encryption_key("zzzz");
        assert!(result.is_err());
    }

    #[test]
    fn parse_encryption_key_wrong_length() {
        // Valid hex but only 30 bytes.
        let hex = "aa".repeat(30);
        let result = parse_encryption_key(&hex);
        assert!(result.is_err());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn seed_signing_key_inserts_one_key(pool: sqlx::PgPool) {
        let enc_key = [0u8; 32];
        seed_signing_key(&pool, &enc_key).await.expect("seed ok");

        let repo = SigningKeyRepository::new(&pool);
        let keys = repo.get_active().await.expect("query ok");
        assert_eq!(
            keys.len(),
            1,
            "exactly one key should be present after seed"
        );

        // kid must be a valid UUID.
        keys[0]
            .kid
            .parse::<uuid::Uuid>()
            .expect("kid must be a UUID");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn seed_signing_key_is_idempotent(pool: sqlx::PgPool) {
        let enc_key = [0u8; 32];
        seed_signing_key(&pool, &enc_key)
            .await
            .expect("first seed ok");
        seed_signing_key(&pool, &enc_key)
            .await
            .expect("second seed ok");

        let repo = SigningKeyRepository::new(&pool);
        let keys = repo.get_active().await.expect("query ok");
        assert_eq!(keys.len(), 1, "second call must not insert a duplicate key");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn seed_signing_key_concurrent_calls_produce_one_key(pool: sqlx::PgPool) {
        // Simulate two instances racing to seed simultaneously.
        // Both are given the same pool so they share the same Postgres instance.
        let enc_key = [0u8; 32];
        let pool2 = pool.clone();
        let (r1, r2) = tokio::join!(
            seed_signing_key(&pool, &enc_key),
            seed_signing_key(&pool2, &enc_key),
        );
        r1.expect("first concurrent seed ok");
        r2.expect("second concurrent seed ok");

        let repo = SigningKeyRepository::new(&pool);
        let keys = repo.get_active().await.expect("query ok");
        assert_eq!(
            keys.len(),
            1,
            "concurrent seeds must produce exactly one key, got {}",
            keys.len()
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn seed_signing_key_private_key_decrypts(pool: sqlx::PgPool) {
        let enc_key = [7u8; 32];
        seed_signing_key(&pool, &enc_key).await.expect("seed ok");

        let repo = SigningKeyRepository::new(&pool);
        let key = repo.get_newest_active().await.unwrap().unwrap();

        // The stored blob must decrypt successfully with the same key.
        let plaintext = crate::crypto::encrypt::decrypt_private_key(&key.private_key, &enc_key)
            .expect("decryption must succeed");
        assert!(!plaintext.is_empty());
    }
}
