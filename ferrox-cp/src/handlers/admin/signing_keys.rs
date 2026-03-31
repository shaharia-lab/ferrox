use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Serialize;
use tracing::{error, info};

use crate::crypto::encrypt::encrypt_private_key;
use crate::crypto::keys::generate_keypair;
use crate::db::audit_repo::AuditRepository;
use crate::db::models::AuditEvent;
use crate::db::signing_key_repo::SigningKeyRepository;
use crate::state::CpState;

// ── Response types ────────────────────────────────────────────────────────────

/// Public metadata for a signing key — no key material.
#[derive(Debug, Serialize)]
pub struct SigningKeyResponse {
    pub kid: String,
    pub algorithm: String,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub retired_at: Option<DateTime<Utc>>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// `GET /api/signing-keys`
///
/// Lists all signing keys (active and retired).  Private key material is never
/// included in the response.
pub async fn list_signing_keys(
    State(state): State<CpState>,
) -> Result<Json<Vec<SigningKeyResponse>>, (StatusCode, Json<serde_json::Value>)> {
    let repo = SigningKeyRepository::new(&state.db);
    let keys = repo.get_all().await.map_err(|e| {
        error!(error = %e, "db error listing signing keys");
        api_error(StatusCode::INTERNAL_SERVER_ERROR, "database error")
    })?;

    Ok(Json(keys.into_iter().map(key_to_response).collect()))
}

/// `POST /api/signing-keys/rotate`
///
/// Generates a new RSA-2048 keypair and schedules the current active key(s)
/// for retirement after a grace period equal to the longest client token TTL.
/// Both keys remain in the JWKS during the overlap window so in-flight tokens
/// stay verifiable.
pub async fn rotate_keys(
    State(state): State<CpState>,
) -> Result<(StatusCode, Json<SigningKeyResponse>), (StatusCode, Json<serde_json::Value>)> {
    let enc_key = crate::parse_encryption_key(&state.config.cp_encryption_key).map_err(|e| {
        error!(error = %e, "failed to parse encryption key");
        api_error(StatusCode::INTERNAL_SERVER_ERROR, "configuration error")
    })?;

    let key_repo = SigningKeyRepository::new(&state.db);

    // Determine grace period from the longest token TTL across all active clients.
    let max_ttl_secs: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(token_ttl_seconds), 900)::bigint FROM clients WHERE active = true",
    )
    .fetch_one(&state.db)
    .await
    .map_err(|e| {
        error!(error = %e, "failed to query max TTL");
        api_error(StatusCode::INTERNAL_SERVER_ERROR, "database error")
    })?;

    // Snapshot current active keys before inserting the new one.
    let current_active = key_repo.get_active().await.map_err(|e| {
        error!(error = %e, "failed to list active keys");
        api_error(StatusCode::INTERNAL_SERVER_ERROR, "database error")
    })?;

    // Generate and persist the new keypair.
    let kp = generate_keypair().map_err(|e| {
        error!(error = %e, "keypair generation failed");
        api_error(StatusCode::INTERNAL_SERVER_ERROR, "key generation failed")
    })?;
    let encrypted = encrypt_private_key(&kp.private_key_der, &enc_key);
    let new_key = key_repo
        .create(&kp.kid, &encrypted, &kp.public_key_der)
        .await
        .map_err(|e| {
            error!(error = %e, "failed to persist new signing key");
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "database error")
        })?;

    // Schedule retirement of all previously active keys.
    let retire_at = Utc::now() + chrono::Duration::seconds(max_ttl_secs);
    for old_key in &current_active {
        if let Err(e) = key_repo.schedule_retirement(&old_key.kid, retire_at).await {
            error!(kid = %old_key.kid, error = %e, "failed to schedule key retirement");
        }
    }

    // Audit log — non-fatal.
    let audit_meta = serde_json::json!({
        "new_kid": &new_key.kid,
        "retired_kids": current_active.iter().map(|k| k.kid.as_str()).collect::<Vec<_>>(),
    });
    if let Err(e) = AuditRepository::new(&state.db)
        .record(None, &AuditEvent::KeyRotated, Some(&audit_meta))
        .await
    {
        error!(error = %e, "failed to write key_rotated audit entry");
    }

    info!(new_kid = %new_key.kid, retired_count = current_active.len(), "signing keys rotated");

    Ok((StatusCode::CREATED, Json(key_to_response(new_key))))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn key_to_response(k: crate::db::models::SigningKey) -> SigningKeyResponse {
    SigningKeyResponse {
        kid: k.kid,
        algorithm: k.algorithm,
        active: k.active,
        created_at: k.created_at,
        retired_at: k.retired_at,
    }
}

fn api_error(status: StatusCode, msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        status,
        Json(serde_json::json!({"error": status.as_str(), "message": msg})),
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::{get, post};
    use axum::Router;
    use std::sync::Arc;
    use tower::ServiceExt;

    use crate::config::CpConfig;
    use crate::crypto::encrypt::encrypt_private_key;
    use crate::crypto::keys::generate_keypair;
    use crate::middleware::admin_auth::require_admin_key;

    const ADMIN_KEY: &str = "test-admin-key-for-sk-tests";
    const TEST_ENC_KEY_HEX: &str =
        "0000000000000000000000000000000000000000000000000000000000000000";

    fn make_state(pool: sqlx::PgPool) -> CpState {
        CpState {
            db: pool,
            config: Arc::new(CpConfig {
                database_url: String::new(),
                cp_issuer: "https://ferrox-cp".to_string(),
                cp_encryption_key: TEST_ENC_KEY_HEX.to_string(),
                admin_key: ADMIN_KEY.to_string(),
                port: 9090,
            }),
        }
    }

    async fn insert_signing_key(pool: &sqlx::PgPool) -> String {
        let kp = generate_keypair().unwrap();
        let enc_key = [0u8; 32];
        let encrypted = encrypt_private_key(&kp.private_key_der, &enc_key);
        sqlx::query("INSERT INTO signing_keys (kid, private_key, public_key) VALUES ($1, $2, $3)")
            .bind(&kp.kid)
            .bind(&encrypted)
            .bind(&kp.public_key_der)
            .execute(pool)
            .await
            .unwrap();
        kp.kid
    }

    fn make_app(state: CpState) -> Router {
        let admin_routes = Router::new()
            .route("/api/signing-keys", get(list_signing_keys))
            .route("/api/signing-keys/rotate", post(rotate_keys))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                require_admin_key,
            ));
        Router::new().merge(admin_routes).with_state(state)
    }

    fn admin_req(method: &str, uri: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("Authorization", format!("Bearer {ADMIN_KEY}"))
            .body(Body::empty())
            .unwrap()
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn list_returns_all_keys(pool: sqlx::PgPool) {
        let kid = insert_signing_key(&pool).await;
        let app = make_app(make_state(pool));

        let resp = app
            .oneshot(admin_req("GET", "/api/signing-keys"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
                .unwrap();
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["kid"], kid);
        assert!(
            !arr[0].as_object().unwrap().contains_key("private_key"),
            "private key must not be exposed"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn rotate_creates_new_key_and_schedules_old_retirement(pool: sqlx::PgPool) {
        let old_kid = insert_signing_key(&pool).await;
        let app = make_app(make_state(pool.clone()));

        let resp = app
            .oneshot(admin_req("POST", "/api/signing-keys/rotate"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
                .unwrap();
        let new_kid = body["kid"].as_str().unwrap();
        assert_ne!(new_kid, old_kid, "rotation must produce a different kid");
        assert!(body["active"].as_bool().unwrap());

        // Both keys should still be active (old key is scheduled, not yet retired).
        let all_active = SigningKeyRepository::new(&pool).get_active().await.unwrap();
        assert_eq!(
            all_active.len(),
            2,
            "both keys should be active during overlap window"
        );

        // Old key must have a retirement timestamp set.
        let all_keys = SigningKeyRepository::new(&pool).get_all().await.unwrap();
        let old_key = all_keys.iter().find(|k| k.kid == old_kid).unwrap();
        assert!(
            old_key.retired_at.is_some(),
            "old key must have retired_at scheduled"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn rotate_writes_audit_log(pool: sqlx::PgPool) {
        insert_signing_key(&pool).await;
        let app = make_app(make_state(pool.clone()));

        app.oneshot(admin_req("POST", "/api/signing-keys/rotate"))
            .await
            .unwrap();

        let entries = crate::db::audit_repo::AuditRepository::new(&pool)
            .list(crate::db::audit_repo::AuditFilter {
                event: Some(AuditEvent::KeyRotated),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn retire_expired_deactivates_keys_past_scheduled_time(pool: sqlx::PgPool) {
        // Insert a key and immediately schedule its retirement in the past.
        let kid = insert_signing_key(&pool).await;
        let past = Utc::now() - chrono::Duration::seconds(1);
        SigningKeyRepository::new(&pool)
            .schedule_retirement(&kid, past)
            .await
            .unwrap();

        // Background task equivalent.
        let retired = SigningKeyRepository::new(&pool)
            .retire_expired()
            .await
            .unwrap();
        assert_eq!(retired, 1);

        let active = SigningKeyRepository::new(&pool).get_active().await.unwrap();
        assert!(
            active.is_empty(),
            "key should be deactivated after retire_expired"
        );
    }
}
