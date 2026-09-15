// ferrox-cp: control plane for the Ferrox LLM gateway
// Phase 3 — public API + admin API + embedded admin UI.
#![allow(dead_code)]
mod budget;
mod config;
mod crypto;
mod db;
mod error;
mod handlers;
mod middleware;
mod openapi;
mod state;
mod ui;

use std::sync::Arc;
use std::time::Duration;

use axum::routing::{get, post};
use axum::Router;
use tokio::net::TcpListener;
use tracing::{error, info};

use config::CpConfig;
use crypto::encrypt::encrypt_private_key;
use crypto::keys::generate_keypair;
use db::signing_key_repo::SigningKeyRepository;
use error::CpError;
use handlers::admin::audit::list_audit;
use handlers::admin::clients::{
    client_usage, client_usage_details, create_client, get_client, list_clients, reactivate_client,
    revoke_client, update_client_budget,
};
use handlers::admin::signing_keys::{list_signing_keys, rotate_keys};
use handlers::{health::health_handler, jwks::jwks_handler, token::token_handler};
use middleware::admin_auth::require_admin_key;
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

    // Spawn background task: retire signing keys whose scheduled retirement
    // timestamp has passed.  Runs every 60 seconds.
    {
        let bg_db = state.db.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(60));
            ticker.tick().await; // skip the immediate first tick
            loop {
                ticker.tick().await;
                let repo = SigningKeyRepository::new(&bg_db);
                match repo.retire_expired().await {
                    Ok(0) => {}
                    Ok(n) => info!(count = n, "background: retired expired signing keys"),
                    Err(e) => error!(error = %e, "background: key retirement failed"),
                }
            }
        });
    }

    // Spawn background task: check token budgets and revoke over-budget clients.
    // Runs every 60 seconds.
    budget::spawn_budget_checker(state.db.clone(), Duration::from_secs(60));

    let app = build_router(state);

    let addr = format!("0.0.0.0:{}", config.port);
    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind {}: {}", addr, e))?;

    info!(addr = %addr, "ferrox-cp listening");
    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("server error: {}", e))
}

/// Build the full control-plane router: public routes, admin routes behind
/// `CP_ADMIN_KEY`, and the embedded UI as the fallback.
fn build_router(state: CpState) -> Router {
    // ── Public routes (no auth) ──────────────────────────────────────────────
    let public_routes = Router::new()
        .route("/.well-known/jwks.json", get(jwks_handler))
        .route("/token", post(token_handler))
        .route("/healthz", get(health_handler))
        // OpenAPI schema for the REST API. Cold, unauthenticated;
        // `/openapi.json` is the auto-detected convention, `/api-schema` a
        // friendly alias.
        .route("/api-schema", get(schema_handler))
        .route("/openapi.json", get(schema_handler));

    // ── Admin routes (CP_ADMIN_KEY required) ────────────────────────────────
    let admin_routes = Router::new()
        .route("/api/clients", post(create_client).get(list_clients))
        .route("/api/clients/:id", get(get_client).delete(revoke_client))
        .route("/api/clients/:id/usage", get(client_usage))
        .route("/api/clients/:id/usage/details", get(client_usage_details))
        .route(
            "/api/clients/:id/budget",
            axum::routing::patch(update_client_budget),
        )
        .route("/api/clients/:id/reactivate", post(reactivate_client))
        .route("/api/signing-keys", get(list_signing_keys))
        .route("/api/signing-keys/rotate", post(rotate_keys))
        .route("/api/audit", get(list_audit))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_admin_key,
        ));

    Router::new()
        .merge(public_routes)
        .merge(admin_routes)
        .fallback(ui::serve_spa)
        .with_state(state)
}

/// Serve the pre-built OpenAPI document as `application/json`. The body is
/// cached (built once), so this route allocates nothing per request.
async fn schema_handler() -> impl axum::response::IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        openapi::openapi_json(),
    )
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

    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use tower::ServiceExt;

    /// The full router over a lazy pool that never connects — testable without
    /// a database for routes that answer before touching it.
    fn router_without_db() -> Router {
        build_router(CpState {
            db: sqlx::PgPool::connect_lazy("postgres://unused@localhost/unused").unwrap(),
            config: Arc::new(CpConfig {
                database_url: String::new(),
                cp_issuer: "https://ferrox-cp".to_string(),
                cp_encryption_key: "0".repeat(64),
                admin_key: "test-admin-key".to_string(),
                port: 9090,
            }),
        })
    }

    /// Ties the schema's per-route security to the real router: every operation
    /// the document marks as secured must exist and reject an unauthenticated
    /// request (an unknown path would fall through to the UI with a 200).
    #[tokio::test]
    async fn secured_schema_operations_reject_unauthenticated_requests() {
        let app = router_without_db();
        let doc: serde_json::Value = serde_json::from_str(openapi::openapi_json()).unwrap();
        let mut checked = 0;
        for (path, item) in doc["paths"].as_object().unwrap() {
            for (method, op) in item.as_object().unwrap() {
                if op["security"].is_null() {
                    continue;
                }
                let uri = path.replace("{id}", &uuid::Uuid::new_v4().to_string());
                let resp = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method(method.to_uppercase().as_str())
                            .uri(&uri)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let status = resp.status();
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                assert_eq!(
                    status,
                    StatusCode::UNAUTHORIZED,
                    "{} {path} must require auth",
                    method.to_uppercase()
                );
                let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(
                    v["error"],
                    "unauthorized",
                    "{} {path}",
                    method.to_uppercase()
                );
                checked += 1;
            }
        }
        // 11 admin operations + `POST /token`.
        assert_eq!(checked, 12);
    }

    #[tokio::test]
    async fn schema_routes_serve_openapi_json_without_auth() {
        let app = router_without_db();

        for path in ["/api-schema", "/openapi.json"] {
            let resp = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "{path} should be public + 200"
            );
            let content_type = resp.headers()[header::CONTENT_TYPE].to_str().unwrap();
            assert!(
                content_type.starts_with("application/json"),
                "{path} content-type was {content_type}"
            );
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let doc: serde_json::Value = serde_json::from_slice(&body)
                .unwrap_or_else(|e| panic!("{path} body must be JSON: {e}"));
            assert!(
                doc["openapi"]
                    .as_str()
                    .unwrap_or_default()
                    .starts_with("3."),
                "{path} must be an OpenAPI 3.x document"
            );
            assert!(doc["paths"]["/api/clients"].is_object());
        }
    }

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
