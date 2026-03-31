use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{error, info};
use uuid::Uuid;

use crate::db::audit_repo::AuditRepository;
use crate::db::client_repo::ClientRepository;
use crate::db::models::AuditEvent;
use crate::state::CpState;

// ── Request / response types ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateClientRequest {
    pub name: String,
    pub description: Option<String>,
    pub allowed_models: Vec<String>,
    pub rpm: i32,
    pub burst: i32,
    pub token_ttl_seconds: i32,
}

/// Response for `POST /api/clients`.  Includes `api_key` which is shown **once**.
#[derive(Debug, Serialize)]
pub struct CreateClientResponse {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    /// Plaintext API key — shown exactly once on creation.
    pub api_key: String,
    pub allowed_models: Vec<String>,
    pub rpm: i32,
    pub burst: i32,
    pub token_ttl_seconds: i32,
    pub active: bool,
    pub created_at: DateTime<Utc>,
}

/// Safe client representation (no key material).
#[derive(Debug, Serialize)]
pub struct ClientResponse {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub allowed_models: Vec<String>,
    pub rpm: i32,
    pub burst: i32,
    pub token_ttl_seconds: i32,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct UsageResponse {
    pub last_24h: i64,
    pub last_7d: i64,
    pub last_30d: i64,
}

#[derive(Debug, Deserialize)]
pub struct PaginationParams {
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

fn default_limit() -> i64 {
    50
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// `POST /api/clients`
///
/// Creates a new API client.  Generates an `sk-cp-` prefixed key, hashes it
/// with bcrypt, stores the hash, and returns the plaintext key once in the
/// response body.
pub async fn create_client(
    State(state): State<CpState>,
    Json(req): Json<CreateClientRequest>,
) -> Result<(StatusCode, Json<CreateClientResponse>), (StatusCode, Json<serde_json::Value>)> {
    // Generate a 32-char base64url body → sk-cp-<32 chars>.
    let random_bytes: Vec<u8> = {
        use rand::RngCore;
        let mut buf = vec![0u8; 24]; // 24 raw bytes → 32 base64url chars
        rand::thread_rng().fill_bytes(&mut buf);
        buf
    };
    let key_body = URL_SAFE_NO_PAD.encode(&random_bytes);
    let raw_key = format!("sk-cp-{}", key_body);
    let key_prefix = key_body[..8].to_string();

    // bcrypt is CPU-bound — run on a blocking thread.
    let raw_key_for_hash = raw_key.clone();
    let hash = tokio::task::spawn_blocking(move || {
        bcrypt::hash(raw_key_for_hash.as_bytes(), bcrypt::DEFAULT_COST)
    })
    .await
    .map_err(|_| api_error(StatusCode::INTERNAL_SERVER_ERROR, "bcrypt task panicked"))?
    .map_err(|e| {
        error!(error = %e, "bcrypt hash failed");
        api_error(StatusCode::INTERNAL_SERVER_ERROR, "key hashing failed")
    })?;

    let repo = ClientRepository::new(&state.db);
    let client = repo
        .create(
            &req.name,
            req.description.as_deref(),
            &key_prefix,
            &hash,
            &req.allowed_models,
            req.rpm,
            req.burst,
            req.token_ttl_seconds,
        )
        .await
        .map_err(|e| {
            if matches!(e, crate::db::error::RepoError::Conflict(_)) {
                api_error(StatusCode::CONFLICT, &format!("{e}"))
            } else {
                error!(error = %e, "db error creating client");
                api_error(StatusCode::INTERNAL_SERVER_ERROR, "database error")
            }
        })?;

    // Audit log — non-fatal.
    let audit_meta = serde_json::json!({ "name": client.name });
    if let Err(e) = AuditRepository::new(&state.db)
        .record(
            Some(client.id),
            &AuditEvent::ClientCreated,
            Some(&audit_meta),
        )
        .await
    {
        error!(error = %e, "failed to write client_created audit entry");
    }

    info!(client = %client.name, id = %client.id, "client created");

    Ok((
        StatusCode::CREATED,
        Json(CreateClientResponse {
            id: client.id,
            name: client.name,
            description: client.description,
            api_key: raw_key,
            allowed_models: client.allowed_models,
            rpm: client.rpm,
            burst: client.burst,
            token_ttl_seconds: client.token_ttl_seconds,
            active: client.active,
            created_at: client.created_at,
        }),
    ))
}

/// `GET /api/clients`
pub async fn list_clients(
    State(state): State<CpState>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<Vec<ClientResponse>>, (StatusCode, Json<serde_json::Value>)> {
    let repo = ClientRepository::new(&state.db);
    let clients = repo.list(params.limit, params.offset).await.map_err(|e| {
        error!(error = %e, "db error listing clients");
        api_error(StatusCode::INTERNAL_SERVER_ERROR, "database error")
    })?;

    Ok(Json(clients.into_iter().map(client_to_response).collect()))
}

/// `GET /api/clients/:id`
pub async fn get_client(
    State(state): State<CpState>,
    Path(id): Path<Uuid>,
) -> Result<Json<ClientResponse>, (StatusCode, Json<serde_json::Value>)> {
    let repo = ClientRepository::new(&state.db);
    let client = repo.find_by_id(id).await.map_err(|e| {
        error!(error = %e, "db error fetching client");
        api_error(StatusCode::INTERNAL_SERVER_ERROR, "database error")
    })?;

    match client {
        Some(c) => Ok(Json(client_to_response(c))),
        None => Err(api_error(StatusCode::NOT_FOUND, "client not found")),
    }
}

/// `DELETE /api/clients/:id`
pub async fn revoke_client(
    State(state): State<CpState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    let repo = ClientRepository::new(&state.db);
    repo.revoke(id).await.map_err(|e| {
        if matches!(e, crate::db::error::RepoError::NotFound(_)) {
            api_error(StatusCode::NOT_FOUND, "client not found")
        } else {
            error!(error = %e, "db error revoking client");
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "database error")
        }
    })?;

    // Audit log — non-fatal.
    let audit_meta = serde_json::json!({ "client_id": id });
    if let Err(e) = AuditRepository::new(&state.db)
        .record(Some(id), &AuditEvent::ClientRevoked, Some(&audit_meta))
        .await
    {
        error!(error = %e, "failed to write client_revoked audit entry");
    }

    info!(client_id = %id, "client revoked");
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/clients/:id/usage`
pub async fn client_usage(
    State(state): State<CpState>,
    Path(id): Path<Uuid>,
) -> Result<Json<UsageResponse>, (StatusCode, Json<serde_json::Value>)> {
    // Verify client exists first.
    let repo = ClientRepository::new(&state.db);
    if repo
        .find_by_id(id)
        .await
        .map_err(|e| {
            error!(error = %e, "db error checking client");
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "database error")
        })?
        .is_none()
    {
        return Err(api_error(StatusCode::NOT_FOUND, "client not found"));
    }

    let audit_repo = AuditRepository::new(&state.db);
    let now = Utc::now();

    let last_24h = audit_repo
        .count_tokens_issued(id, now - chrono::Duration::hours(24))
        .await
        .map_err(|e| {
            error!(error = %e, "db error counting tokens");
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "database error")
        })?;

    let last_7d = audit_repo
        .count_tokens_issued(id, now - chrono::Duration::days(7))
        .await
        .map_err(|e| {
            error!(error = %e, "db error counting tokens");
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "database error")
        })?;

    let last_30d = audit_repo
        .count_tokens_issued(id, now - chrono::Duration::days(30))
        .await
        .map_err(|e| {
            error!(error = %e, "db error counting tokens");
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "database error")
        })?;

    Ok(Json(UsageResponse {
        last_24h,
        last_7d,
        last_30d,
    }))
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn client_to_response(c: crate::db::models::Client) -> ClientResponse {
    ClientResponse {
        id: c.id,
        name: c.name,
        description: c.description,
        allowed_models: c.allowed_models,
        rpm: c.rpm,
        burst: c.burst,
        token_ttl_seconds: c.token_ttl_seconds,
        active: c.active,
        created_at: c.created_at,
        revoked_at: c.revoked_at,
    }
}

fn api_error(status: StatusCode, msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        status,
        Json(serde_json::json!({"error": status.as_str(), "message": msg})),
    )
}

// ── Tests ────────────────────────────────────────────────────────────────────

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
    use crate::middleware::admin_auth::require_admin_key;

    const ADMIN_KEY: &str = "test-admin-key-for-unit-tests";

    fn make_state(pool: sqlx::PgPool) -> CpState {
        CpState {
            db: pool,
            config: Arc::new(CpConfig {
                database_url: String::new(),
                cp_issuer: "https://ferrox-cp".to_string(),
                cp_encryption_key: "0".repeat(64),
                admin_key: ADMIN_KEY.to_string(),
                port: 9090,
            }),
        }
    }

    fn admin_request(method: &str, uri: &str, body: Option<&str>) -> Request<Body> {
        let builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("Authorization", format!("Bearer {ADMIN_KEY}"))
            .header("Content-Type", "application/json");
        match body {
            Some(b) => builder.body(Body::from(b.to_string())).unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        }
    }

    fn make_app(state: CpState) -> Router {
        let admin_routes = Router::new()
            .route("/api/clients", post(create_client).get(list_clients))
            .route("/api/clients/:id", get(get_client).delete(revoke_client))
            .route("/api/clients/:id/usage", get(client_usage))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                require_admin_key,
            ));
        Router::new().merge(admin_routes).with_state(state)
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn create_client_returns_201_with_api_key(pool: sqlx::PgPool) {
        let app = make_app(make_state(pool));
        let resp = app
            .oneshot(admin_request(
                "POST",
                "/api/clients",
                Some(
                    r#"{"name":"svc","allowed_models":["*"],"rpm":100,"burst":10,"token_ttl_seconds":900}"#,
                ),
            ))
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::CREATED);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
                .unwrap();
        let key = body["api_key"].as_str().unwrap();
        assert!(
            key.starts_with("sk-cp-"),
            "key must start with sk-cp-: {key}"
        );
        assert!(body["id"].is_string());
        assert!(
            !body.as_object().unwrap().contains_key("api_key_hash"),
            "hash must not be exposed"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn create_client_api_key_works_with_find_by_prefix(pool: sqlx::PgPool) {
        let state = make_state(pool.clone());
        let app = make_app(state);
        let resp = app
            .oneshot(admin_request(
                "POST",
                "/api/clients",
                Some(
                    r#"{"name":"pfx-svc","allowed_models":["*"],"rpm":10,"burst":5,"token_ttl_seconds":900}"#,
                ),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
                .unwrap();
        let raw_key = body["api_key"].as_str().unwrap().to_string();

        // The key prefix (first 8 chars after sk-cp-) must index correctly.
        let key_body = raw_key.strip_prefix("sk-cp-").unwrap();
        let prefix = &key_body[..8];
        let found = crate::db::client_repo::ClientRepository::new(&pool)
            .find_by_key_prefix(prefix)
            .await
            .unwrap();
        assert!(found.is_some());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn create_duplicate_name_returns_409(pool: sqlx::PgPool) {
        let app = make_app(make_state(pool));
        let body =
            r#"{"name":"dup","allowed_models":["*"],"rpm":10,"burst":5,"token_ttl_seconds":900}"#;
        let (a, b) = tokio::join!(
            axum::Router::clone(&app).oneshot(admin_request("POST", "/api/clients", Some(body))),
            // We can't reuse app after move; this test calls sequentially via oneshot.
            // Instead just send two requests via separate oneshot on cloned app.
            axum::Router::clone(&app).oneshot(admin_request("POST", "/api/clients", Some(body))),
        );
        let statuses = [a.unwrap().status(), b.unwrap().status()];
        // One must be 201, the other 409.
        assert!(statuses.contains(&StatusCode::CREATED));
        assert!(statuses.contains(&StatusCode::CONFLICT));
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn list_clients_returns_created_clients(pool: sqlx::PgPool) {
        let state = make_state(pool);
        let app = make_app(state);
        // Create two clients.
        for name in &["svc-a", "svc-b"] {
            let body = format!(
                r#"{{"name":"{name}","allowed_models":["*"],"rpm":10,"burst":5,"token_ttl_seconds":900}}"#
            );
            app.clone()
                .oneshot(admin_request("POST", "/api/clients", Some(&body)))
                .await
                .unwrap();
        }

        let resp = app
            .oneshot(admin_request("GET", "/api/clients", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 8192).await.unwrap())
                .unwrap();
        assert_eq!(body.as_array().unwrap().len(), 2);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn get_unknown_client_returns_404(pool: sqlx::PgPool) {
        let app = make_app(make_state(pool));
        let resp = app
            .oneshot(admin_request(
                "GET",
                &format!("/api/clients/{}", Uuid::new_v4()),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn revoke_client_returns_204(pool: sqlx::PgPool) {
        let state = make_state(pool.clone());
        let app = make_app(state);

        // Create a client.
        let resp = app
            .clone()
            .oneshot(admin_request(
                "POST",
                "/api/clients",
                Some(r#"{"name":"to-revoke","allowed_models":["*"],"rpm":10,"burst":5,"token_ttl_seconds":900}"#),
            ))
            .await
            .unwrap();
        let created: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
                .unwrap();
        let id = created["id"].as_str().unwrap();

        // Revoke it.
        let resp = app
            .oneshot(admin_request("DELETE", &format!("/api/clients/{id}"), None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Verify the client is now inactive.
        let uuid: Uuid = id.parse().unwrap();
        let client = crate::db::client_repo::ClientRepository::new(&pool)
            .find_by_id(uuid)
            .await
            .unwrap()
            .unwrap();
        assert!(!client.active);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn missing_admin_key_returns_401(pool: sqlx::PgPool) {
        let app = make_app(make_state(pool));
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/clients")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn wrong_admin_key_returns_401(pool: sqlx::PgPool) {
        let app = make_app(make_state(pool));
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/clients")
                    .header("Authorization", "Bearer wrong-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn client_usage_returns_counts(pool: sqlx::PgPool) {
        let state = make_state(pool.clone());
        let app = make_app(state);

        // Create a client.
        let resp = app
            .clone()
            .oneshot(admin_request(
                "POST",
                "/api/clients",
                Some(r#"{"name":"usage-svc","allowed_models":["*"],"rpm":10,"burst":5,"token_ttl_seconds":900}"#),
            ))
            .await
            .unwrap();
        let created: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
                .unwrap();
        let id: Uuid = created["id"].as_str().unwrap().parse().unwrap();

        // Insert a few token_issued events.
        let audit = crate::db::audit_repo::AuditRepository::new(&pool);
        for _ in 0..3 {
            audit
                .record(Some(id), &AuditEvent::TokenIssued, None)
                .await
                .unwrap();
        }

        let resp = app
            .oneshot(admin_request(
                "GET",
                &format!("/api/clients/{id}/usage"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1024).await.unwrap())
                .unwrap();
        assert_eq!(body["last_24h"].as_i64().unwrap(), 3);
        assert_eq!(body["last_7d"].as_i64().unwrap(), 3);
        assert_eq!(body["last_30d"].as_i64().unwrap(), 3);
    }
}
