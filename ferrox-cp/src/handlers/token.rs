use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Serialize;
use tracing::{error, info, warn};

use crate::crypto::encrypt::decrypt_private_key;
use crate::crypto::jwt::JwtSigner;
use crate::db::audit_repo::AuditRepository;
use crate::db::client_repo::ClientRepository;
use crate::db::models::AuditEvent;
use crate::db::signing_key_repo::SigningKeyRepository;
use crate::error::CpError;
use crate::state::CpState;

const API_KEY_PREFIX: &str = "sk-cp-";

/// A pre-computed bcrypt hash used as a dummy target when no client is found
/// for a given key prefix.  Running bcrypt against it on every miss ensures
/// the response time is indistinguishable from a genuine hash mismatch,
/// preventing timing-based prefix enumeration.
///
/// This is the hash of the string "dummy" with cost 12.
const DUMMY_HASH: &str = "$2b$12$Ei1YpGUfDLEH.8ZhFDcKMucYanSmS6.v.roB0DEjxFKnKhMBVFjFC";

#[derive(Debug, Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: &'static str,
    pub expires_in: i64,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: &'static str,
    pub message: String,
}

/// `POST /token`
///
/// Exchanges a static client API key for a short-lived JWT.
///
/// The request must carry `Authorization: Bearer sk-cp-<key>`.
/// The key is verified with bcrypt against the stored hash; on success a
/// signed JWT is returned and an audit log entry is written.
pub async fn token_handler(
    State(state): State<CpState>,
    headers: HeaderMap,
) -> Result<Json<TokenResponse>, (StatusCode, Json<ErrorResponse>)> {
    // ── 1. Extract and validate the Bearer token ────────────────────────────
    let raw_key = extract_bearer_key(&headers)?;

    if !raw_key.starts_with(API_KEY_PREFIX) {
        return Err(unauthorized("invalid API key format"));
    }

    // ── 2. Look up client by key prefix (first 8 chars after the sk-cp- prefix)
    let full_key = raw_key; // e.g. "sk-cp-abcd1234restofkey"
    let key_body = &full_key[API_KEY_PREFIX.len()..]; // strip "sk-cp-"
    if key_body.len() < 8 {
        return Err(unauthorized("API key too short"));
    }
    let prefix = &key_body[..8];

    let client_repo = ClientRepository::new(&state.db);
    let maybe_client = match client_repo.find_by_key_prefix(prefix).await {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "database error looking up client by prefix");
            return Err(internal_error());
        }
    };

    // ── 3. Full bcrypt verification ─────────────────────────────────────────
    // Always run bcrypt — even when no client was found — to prevent a timing
    // oracle that would let an attacker enumerate valid key prefixes by
    // measuring whether the response took ~0 ms (miss) or ~100 ms (bcrypt).
    let hash = maybe_client
        .as_ref()
        .map(|c| c.api_key_hash.clone())
        .unwrap_or_else(|| DUMMY_HASH.to_string());
    let key_to_verify = full_key.clone();
    let valid = tokio::task::spawn_blocking(move || {
        bcrypt::verify(key_to_verify.as_bytes(), &hash).unwrap_or(false)
    })
    .await
    .map_err(|_| internal_error())?;

    let client = match maybe_client {
        Some(c) if valid => c,
        _ => {
            warn!("bcrypt verification failed or client not found");
            return Err(unauthorized("invalid API key"));
        }
    };

    // ── 4. Guard against revoked clients ───────────────────────────────────
    if !client.active || client.revoked_at.is_some() {
        warn!(client = %client.name, "token request for revoked client");
        return Err(unauthorized("client is revoked"));
    }

    // ── 5. Load the newest active signing key and build a JwtSigner ────────
    let key_repo = SigningKeyRepository::new(&state.db);
    let signing_key = match key_repo.get_newest_active().await {
        Ok(Some(k)) => k,
        Ok(None) => {
            error!("no active signing key available");
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse {
                    error: "service_unavailable",
                    message: "no active signing key".to_string(),
                }),
            ));
        }
        Err(e) => {
            error!(error = %e, "failed to load signing key");
            return Err(internal_error());
        }
    };

    let enc_key = match crate::parse_encryption_key(&state.config.cp_encryption_key) {
        Ok(k) => k,
        Err(e) => {
            error!(error = %e, "failed to parse encryption key");
            return Err(internal_error());
        }
    };
    let private_key_der = match decrypt_private_key(&signing_key.private_key, &enc_key) {
        Ok(k) => k,
        Err(e) => {
            error!(error = %e, "failed to decrypt signing key");
            return Err(internal_error());
        }
    };

    let signer = match JwtSigner::new(
        &signing_key,
        &private_key_der,
        state.config.cp_issuer.clone(),
    ) {
        Ok(s) => s,
        Err(CpError::JwtSigning(msg)) => {
            error!(error = %msg, "failed to create JWT signer");
            return Err(internal_error());
        }
        Err(e) => {
            error!(error = %e, "unexpected error creating JWT signer");
            return Err(internal_error());
        }
    };

    // ── 6. Sign the token ───────────────────────────────────────────────────
    let signed = match signer.sign(&client) {
        Ok(t) => t,
        Err(e) => {
            error!(error = %e, client = %client.name, "JWT signing failed");
            return Err(internal_error());
        }
    };

    // ── 7. Write audit log ──────────────────────────────────────────────────
    let expires_at = signed.expires_at;
    let jti = signed.jti.clone();
    let audit_repo = AuditRepository::new(&state.db);
    let audit_meta = serde_json::json!({ "jti": jti, "exp": expires_at });
    if let Err(e) = audit_repo
        .record(Some(client.id), &AuditEvent::TokenIssued, Some(&audit_meta))
        .await
    {
        // Audit failure is non-fatal — log and continue.
        error!(error = %e, "failed to write audit log entry");
    }

    let expires_in = expires_at - chrono::Utc::now().timestamp();

    info!(client = %client.name, jti = %signed.jti, "token issued");

    Ok(Json(TokenResponse {
        access_token: signed.token,
        token_type: "Bearer",
        expires_in,
    }))
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn extract_bearer_key(headers: &HeaderMap) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| unauthorized("missing Authorization header"))?;

    let token = auth
        .strip_prefix("Bearer ")
        .ok_or_else(|| unauthorized("Authorization header must use Bearer scheme"))?;

    Ok(token.to_string())
}

fn unauthorized(msg: &str) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::UNAUTHORIZED,
        Json(ErrorResponse {
            error: "unauthorized",
            message: msg.to_string(),
        }),
    )
}

fn internal_error() -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
            error: "internal_error",
            message: "an unexpected error occurred".to_string(),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::post;
    use axum::Router;
    use std::sync::Arc;
    use tower::ServiceExt;

    use crate::config::CpConfig;
    use crate::crypto::encrypt::encrypt_private_key;
    use crate::crypto::keys::generate_keypair;

    const TEST_ENC_KEY_HEX: &str =
        "0000000000000000000000000000000000000000000000000000000000000000";

    fn make_state(pool: sqlx::PgPool) -> CpState {
        CpState {
            db: pool,
            config: Arc::new(CpConfig {
                database_url: String::new(),
                cp_issuer: "https://ferrox-cp".to_string(),
                cp_encryption_key: TEST_ENC_KEY_HEX.to_string(),
                admin_key: "secret".to_string(),
                port: 9090,
            }),
        }
    }

    /// Insert an active signing key into the pool with the test encryption key.
    async fn insert_signing_key(pool: &sqlx::PgPool) {
        let kp = generate_keypair().expect("keygen ok");
        let enc_key = [0u8; 32];
        let encrypted = encrypt_private_key(&kp.private_key_der, &enc_key);
        sqlx::query("INSERT INTO signing_keys (kid, private_key, public_key) VALUES ($1, $2, $3)")
            .bind(&kp.kid)
            .bind(&encrypted)
            .bind(&kp.public_key_der)
            .execute(pool)
            .await
            .unwrap();
    }

    /// Insert an active client with a known raw key and return the raw key.
    /// `raw_key` must start with "sk-cp-" and have at least 8 chars after the prefix.
    async fn insert_client(pool: &sqlx::PgPool, name: &str, raw_key: &str) -> String {
        let key_body = raw_key
            .strip_prefix(API_KEY_PREFIX)
            .expect("key must start with sk-cp-");
        let prefix = &key_body[..8];
        let hash = bcrypt::hash(raw_key.as_bytes(), bcrypt::DEFAULT_COST).unwrap();
        ClientRepository::new(pool)
            .create(name, None, prefix, &hash, &["*".to_string()], 100, 10, 900)
            .await
            .unwrap();
        raw_key.to_string()
    }

    fn token_request(bearer: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/token")
            .header("Authorization", format!("Bearer {bearer}"))
            .body(Body::empty())
            .unwrap()
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn valid_key_returns_jwt(pool: sqlx::PgPool) {
        insert_signing_key(&pool).await;
        let raw_key = format!("sk-cp-{}rest", "abcd1234");
        insert_client(&pool, "test-client", &raw_key).await;

        let app = Router::new()
            .route("/token", post(token_handler))
            .with_state(make_state(pool));

        let resp = app.oneshot(token_request(&raw_key)).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
                .unwrap();
        assert!(body["access_token"].is_string());
        assert_eq!(body["token_type"], "Bearer");
        assert!(body["expires_in"].as_i64().unwrap() > 0);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn wrong_key_returns_401(pool: sqlx::PgPool) {
        insert_signing_key(&pool).await;
        insert_client(&pool, "another-client", "sk-cp-abcd1234correct").await;

        let app = Router::new()
            .route("/token", post(token_handler))
            .with_state(make_state(pool));

        // Same prefix, different suffix — bcrypt check will fail.
        let resp = app
            .oneshot(token_request("sk-cp-abcd1234wrongsuffix"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn missing_auth_header_returns_401(pool: sqlx::PgPool) {
        let app = Router::new()
            .route("/token", post(token_handler))
            .with_state(make_state(pool));

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn wrong_key_format_returns_401(pool: sqlx::PgPool) {
        let app = Router::new()
            .route("/token", post(token_handler))
            .with_state(make_state(pool));

        // Key without sk-cp- prefix.
        let resp = app
            .oneshot(token_request("invalid-key-format"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn revoked_client_returns_401(pool: sqlx::PgPool) {
        insert_signing_key(&pool).await;
        let raw_key = "sk-cp-revk1234secret";
        insert_client(&pool, "revoked-client", raw_key).await;

        // Revoke the client.
        let repo = ClientRepository::new(&pool);
        let clients = repo.list(1, 0).await.unwrap();
        repo.revoke(clients[0].id).await.unwrap();

        let app = Router::new()
            .route("/token", post(token_handler))
            .with_state(make_state(pool));

        let resp = app.oneshot(token_request(raw_key)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn no_signing_key_returns_503(pool: sqlx::PgPool) {
        let raw_key = "sk-cp-nokey12secret";
        insert_client(&pool, "no-key-client", raw_key).await;
        // No signing key inserted.

        let app = Router::new()
            .route("/token", post(token_handler))
            .with_state(make_state(pool));

        let resp = app.oneshot(token_request(raw_key)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn successful_token_writes_audit_log(pool: sqlx::PgPool) {
        insert_signing_key(&pool).await;
        let raw_key = "sk-cp-audt1234secret";
        insert_client(&pool, "audit-client", raw_key).await;

        let app = Router::new()
            .route("/token", post(token_handler))
            .with_state(make_state(pool.clone()));

        let resp = app.oneshot(token_request(raw_key)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Verify audit entry was written.
        let audit_repo = AuditRepository::new(&pool);
        let entries = audit_repo
            .list(crate::db::audit_repo::AuditFilter {
                client_id: None,
                event: Some(AuditEvent::TokenIssued),
                since: None,
                limit: Some(10),
            })
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event, AuditEvent::TokenIssued);
    }
}
