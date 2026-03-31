use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;
use tracing::error;

use crate::crypto::jwks::{public_key_to_jwk, Jwk};
use crate::db::signing_key_repo::SigningKeyRepository;
use crate::state::CpState;

#[derive(Debug, Serialize)]
pub struct JwksResponse {
    pub keys: Vec<Jwk>,
}

/// `GET /.well-known/jwks.json`
///
/// Returns all active RSA public keys in JWKS format (RFC 7517).
/// No authentication required — this endpoint is called by the gateway.
/// Response is cacheable for 5 minutes via `Cache-Control: max-age=300`.
pub async fn jwks_handler(State(state): State<CpState>) -> impl IntoResponse {
    let repo = SigningKeyRepository::new(&state.db);

    let active_keys = match repo.get_active().await {
        Ok(keys) => keys,
        Err(e) => {
            error!(error = %e, "failed to fetch active signing keys");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CACHE_CONTROL, "no-store")],
                Json(JwksResponse { keys: vec![] }),
            )
                .into_response();
        }
    };

    let mut jwks = Vec::with_capacity(active_keys.len());
    for key in &active_keys {
        match public_key_to_jwk(&key.kid, &key.public_key) {
            Ok(jwk) => jwks.push(jwk),
            Err(e) => {
                error!(kid = %key.kid, error = %e, "failed to serialise public key to JWK");
            }
        }
    }

    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "max-age=300, public")],
        Json(JwksResponse { keys: jwks }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use std::sync::Arc;
    use tower::ServiceExt;

    use crate::config::CpConfig;
    use crate::crypto::encrypt::encrypt_private_key;
    use crate::crypto::keys::generate_keypair;

    fn make_state(pool: sqlx::PgPool) -> CpState {
        CpState {
            db: pool,
            config: Arc::new(CpConfig {
                database_url: String::new(),
                cp_issuer: "https://ferrox-cp".to_string(),
                cp_encryption_key: "a".repeat(64),
                admin_key: "secret".to_string(),
                port: 9090,
            }),
        }
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn jwks_returns_active_keys(pool: sqlx::PgPool) {
        // Insert an active signing key.
        let kp = generate_keypair().expect("keygen ok");
        let enc_key = [0u8; 32];
        let encrypted = encrypt_private_key(&kp.private_key_der, &enc_key);
        sqlx::query("INSERT INTO signing_keys (kid, private_key, public_key) VALUES ($1, $2, $3)")
            .bind(&kp.kid)
            .bind(&encrypted)
            .bind(&kp.public_key_der)
            .execute(&pool)
            .await
            .unwrap();

        let state = make_state(pool);
        let app = Router::new()
            .route("/.well-known/jwks.json", get(jwks_handler))
            .with_state(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/.well-known/jwks.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);

        // Verify Cache-Control header.
        let cc = resp.headers().get(header::CACHE_CONTROL).unwrap();
        assert!(cc.to_str().unwrap().contains("max-age=300"));

        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
                .unwrap();
        let keys = body["keys"].as_array().unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0]["kid"], kp.kid.as_str());
        assert_eq!(keys[0]["kty"], "RSA");
        assert_eq!(keys[0]["alg"], "RS256");
        assert!(keys[0]["n"].is_string());
        assert!(keys[0]["e"].is_string());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn jwks_returns_empty_when_no_keys(pool: sqlx::PgPool) {
        let state = make_state(pool);
        let app = Router::new()
            .route("/.well-known/jwks.json", get(jwks_handler))
            .with_state(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/.well-known/jwks.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1024).await.unwrap())
                .unwrap();
        assert_eq!(body["keys"].as_array().unwrap().len(), 0);
    }
}
