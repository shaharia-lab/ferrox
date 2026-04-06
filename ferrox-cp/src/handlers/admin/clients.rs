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
use crate::db::models::{AuditEvent, UsageSummary};
use crate::db::usage_repo::{UsageFilter, UsageRepository};
use crate::state::CpState;

// ── Request / response types ─────────────────────────────────────────────────

const MAX_LIMIT: i64 = 1000;

#[derive(Debug, Deserialize)]
pub struct CreateClientRequest {
    pub name: String,
    pub description: Option<String>,
    pub allowed_models: Vec<String>,
    pub rpm: i32,
    pub burst: i32,
    pub token_ttl_seconds: i32,
    pub token_budget: Option<i64>,
    pub budget_period: Option<String>,
}

impl CreateClientRequest {
    /// Validate the request fields.  Returns an error message if any field is invalid.
    fn validate(&self) -> Result<(), &'static str> {
        if self.name.trim().is_empty() {
            return Err("name must not be empty");
        }
        if self.allowed_models.is_empty() {
            return Err("allowed_models must contain at least one entry");
        }
        if self.rpm <= 0 {
            return Err("rpm must be greater than zero");
        }
        if self.burst <= 0 {
            return Err("burst must be greater than zero");
        }
        if self.token_ttl_seconds <= 0 {
            return Err("token_ttl_seconds must be greater than zero");
        }
        if let Some(budget) = self.token_budget {
            if budget <= 0 {
                return Err("token_budget must be greater than zero");
            }
        }
        if let Some(ref period) = self.budget_period {
            if period != "daily" && period != "monthly" {
                return Err("budget_period must be 'daily' or 'monthly'");
            }
        }
        // Budget and period must be set together.
        if self.token_budget.is_some() != self.budget_period.is_some() {
            return Err("token_budget and budget_period must both be set or both be null");
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
pub struct UpdateBudgetRequest {
    pub token_budget: Option<i64>,
    pub budget_period: Option<String>,
}

impl UpdateBudgetRequest {
    fn validate(&self) -> Result<(), &'static str> {
        if let Some(budget) = self.token_budget {
            if budget <= 0 {
                return Err("token_budget must be greater than zero");
            }
        }
        if let Some(ref period) = self.budget_period {
            if period != "daily" && period != "monthly" {
                return Err("budget_period must be 'daily' or 'monthly'");
            }
        }
        if self.token_budget.is_some() != self.budget_period.is_some() {
            return Err("token_budget and budget_period must both be set or both be null");
        }
        Ok(())
    }
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
    pub token_budget: Option<i64>,
    pub budget_period: Option<String>,
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
    pub token_budget: Option<i64>,
    pub budget_period: Option<String>,
    pub budget_reset_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct UsageResponse {
    pub last_24h: UsageSummary,
    pub last_7d: UsageSummary,
    pub last_30d: UsageSummary,
}

#[derive(Debug, Deserialize)]
pub struct UsageDetailsParams {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub model: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

#[derive(Debug, Serialize)]
pub struct UsageDetailRecord {
    pub request_id: String,
    pub model: String,
    pub provider: String,
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
    pub total_tokens: i32,
    pub latency_ms: Option<i32>,
    pub created_at: DateTime<Utc>,
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
    req.validate()
        .map_err(|msg| api_error(StatusCode::UNPROCESSABLE_ENTITY, msg))?;
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

    // Set budget if provided.
    let client = if req.token_budget.is_some() {
        repo.update_budget(client.id, req.token_budget, req.budget_period.as_deref())
            .await
            .map_err(|e| {
                error!(error = %e, "db error setting budget");
                api_error(StatusCode::INTERNAL_SERVER_ERROR, "database error")
            })?
    } else {
        client
    };

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
            token_budget: client.token_budget,
            budget_period: client.budget_period,
        }),
    ))
}

/// `GET /api/clients`
pub async fn list_clients(
    State(state): State<CpState>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<Vec<ClientResponse>>, (StatusCode, Json<serde_json::Value>)> {
    let limit = params.limit.min(MAX_LIMIT);
    let repo = ClientRepository::new(&state.db);
    let clients = repo.list(limit, params.offset).await.map_err(|e| {
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
///
/// Returns aggregated token usage from `usage_log` for the last 24h, 7d, and 30d.
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

    let usage_repo = UsageRepository::new(&state.db);
    let now = Utc::now();

    let last_24h = usage_repo
        .summarize(id, Some(now - chrono::Duration::hours(24)), Some(now))
        .await
        .map_err(|e| {
            error!(error = %e, "db error summarizing usage");
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "database error")
        })?;

    let last_7d = usage_repo
        .summarize(id, Some(now - chrono::Duration::days(7)), Some(now))
        .await
        .map_err(|e| {
            error!(error = %e, "db error summarizing usage");
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "database error")
        })?;

    let last_30d = usage_repo
        .summarize(id, Some(now - chrono::Duration::days(30)), Some(now))
        .await
        .map_err(|e| {
            error!(error = %e, "db error summarizing usage");
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "database error")
        })?;

    Ok(Json(UsageResponse {
        last_24h,
        last_7d,
        last_30d,
    }))
}

/// `PATCH /api/clients/:id/budget`
///
/// Update token budget settings for a client.
pub async fn update_client_budget(
    State(state): State<CpState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateBudgetRequest>,
) -> Result<Json<ClientResponse>, (StatusCode, Json<serde_json::Value>)> {
    req.validate()
        .map_err(|msg| api_error(StatusCode::UNPROCESSABLE_ENTITY, msg))?;

    let repo = ClientRepository::new(&state.db);
    let client = repo
        .update_budget(id, req.token_budget, req.budget_period.as_deref())
        .await
        .map_err(|e| {
            if matches!(e, crate::db::error::RepoError::NotFound(_)) {
                api_error(StatusCode::NOT_FOUND, "client not found")
            } else {
                error!(error = %e, "db error updating budget");
                api_error(StatusCode::INTERNAL_SERVER_ERROR, "database error")
            }
        })?;

    info!(client_id = %id, budget = ?req.token_budget, period = ?req.budget_period, "budget updated");
    Ok(Json(client_to_response(client)))
}

/// `POST /api/clients/:id/reactivate`
///
/// Re-activate a revoked client and reset its budget period.
pub async fn reactivate_client(
    State(state): State<CpState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    let repo = ClientRepository::new(&state.db);
    repo.reactivate(id).await.map_err(|e| {
        if matches!(e, crate::db::error::RepoError::NotFound(_)) {
            api_error(StatusCode::NOT_FOUND, "client not found or already active")
        } else {
            error!(error = %e, "db error reactivating client");
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "database error")
        }
    })?;

    info!(client_id = %id, "client reactivated");
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/clients/:id/usage/details`
///
/// Returns paginated per-request usage records from `usage_log`.
pub async fn client_usage_details(
    State(state): State<CpState>,
    Path(id): Path<Uuid>,
    Query(params): Query<UsageDetailsParams>,
) -> Result<Json<Vec<UsageDetailRecord>>, (StatusCode, Json<serde_json::Value>)> {
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

    let usage_repo = UsageRepository::new(&state.db);
    let limit = params.limit.min(MAX_LIMIT);
    let records = usage_repo
        .list(UsageFilter {
            client_id: id,
            from: params.from,
            to: params.to,
            model: params.model,
            limit: Some(limit),
            offset: Some(params.offset),
        })
        .await
        .map_err(|e| {
            error!(error = %e, "db error listing usage details");
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "database error")
        })?;

    let response: Vec<UsageDetailRecord> = records
        .into_iter()
        .map(|r| UsageDetailRecord {
            request_id: r.request_id,
            model: r.model,
            provider: r.provider,
            prompt_tokens: r.prompt_tokens,
            completion_tokens: r.completion_tokens,
            total_tokens: r.total_tokens,
            latency_ms: r.latency_ms,
            created_at: r.created_at,
        })
        .collect();

    Ok(Json(response))
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
        token_budget: c.token_budget,
        budget_period: c.budget_period,
        budget_reset_at: c.budget_reset_at,
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
            .route("/api/clients/:id/usage/details", get(client_usage_details))
            .route(
                "/api/clients/:id/budget",
                axum::routing::patch(update_client_budget),
            )
            .route("/api/clients/:id/reactivate", post(reactivate_client))
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
    async fn client_usage_returns_token_summaries(pool: sqlx::PgPool) {
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

        // Insert usage records directly.
        let usage_repo = crate::db::usage_repo::UsageRepository::new(&pool);
        let records: Vec<crate::db::usage_repo::UsageInsert> = (0..3)
            .map(|i| crate::db::usage_repo::UsageInsert {
                client_id: id,
                request_id: format!("req-{i}"),
                model: "gpt-4".to_string(),
                provider: "openai".to_string(),
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                latency_ms: Some(200),
            })
            .collect();
        usage_repo.insert_batch(&records).await.unwrap();

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
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 2048).await.unwrap())
                .unwrap();
        assert_eq!(body["last_24h"]["total_tokens"].as_i64().unwrap(), 450);
        assert_eq!(body["last_24h"]["request_count"].as_i64().unwrap(), 3);
        assert_eq!(body["last_7d"]["total_tokens"].as_i64().unwrap(), 450);
        assert_eq!(body["last_30d"]["total_tokens"].as_i64().unwrap(), 450);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn create_client_empty_name_returns_422(pool: sqlx::PgPool) {
        let app = make_app(make_state(pool));
        let resp = app
            .oneshot(admin_request(
                "POST",
                "/api/clients",
                Some(r#"{"name":"  ","allowed_models":["*"],"rpm":10,"burst":5,"token_ttl_seconds":900}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn create_client_empty_models_returns_422(pool: sqlx::PgPool) {
        let app = make_app(make_state(pool));
        let resp = app
            .oneshot(admin_request(
                "POST",
                "/api/clients",
                Some(r#"{"name":"svc","allowed_models":[],"rpm":10,"burst":5,"token_ttl_seconds":900}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn create_client_zero_rpm_returns_422(pool: sqlx::PgPool) {
        let app = make_app(make_state(pool));
        let resp = app
            .oneshot(admin_request(
                "POST",
                "/api/clients",
                Some(r#"{"name":"svc","allowed_models":["*"],"rpm":0,"burst":5,"token_ttl_seconds":900}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn list_clients_clamps_oversized_limit(pool: sqlx::PgPool) {
        let app = make_app(make_state(pool));
        // A limit larger than MAX_LIMIT must not cause an error — it is silently capped.
        let resp = app
            .oneshot(admin_request("GET", "/api/clients?limit=2147483647", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn client_usage_details_returns_paginated_records(pool: sqlx::PgPool) {
        let state = make_state(pool.clone());
        let app = make_app(state);

        // Create a client.
        let resp = app
            .clone()
            .oneshot(admin_request(
                "POST",
                "/api/clients",
                Some(r#"{"name":"details-svc","allowed_models":["*"],"rpm":10,"burst":5,"token_ttl_seconds":900}"#),
            ))
            .await
            .unwrap();
        let created: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
                .unwrap();
        let id: Uuid = created["id"].as_str().unwrap().parse().unwrap();

        // Insert usage records.
        let usage_repo = crate::db::usage_repo::UsageRepository::new(&pool);
        let records: Vec<crate::db::usage_repo::UsageInsert> = (0..5)
            .map(|i| crate::db::usage_repo::UsageInsert {
                client_id: id,
                request_id: format!("req-{i}"),
                model: if i < 3 {
                    "gpt-4".to_string()
                } else {
                    "claude-3".to_string()
                },
                provider: "openai".to_string(),
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                latency_ms: Some(200),
            })
            .collect();
        usage_repo.insert_batch(&records).await.unwrap();

        // Fetch all details.
        let resp = app
            .clone()
            .oneshot(admin_request(
                "GET",
                &format!("/api/clients/{id}/usage/details"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 8192).await.unwrap())
                .unwrap();
        assert_eq!(body.as_array().unwrap().len(), 5);

        // Filter by model.
        let resp = app
            .clone()
            .oneshot(admin_request(
                "GET",
                &format!("/api/clients/{id}/usage/details?model=gpt-4"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 8192).await.unwrap())
                .unwrap();
        assert_eq!(body.as_array().unwrap().len(), 3);

        // Pagination: limit=2, offset=0.
        let resp = app
            .clone()
            .oneshot(admin_request(
                "GET",
                &format!("/api/clients/{id}/usage/details?limit=2&offset=0"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 8192).await.unwrap())
                .unwrap();
        assert_eq!(body.as_array().unwrap().len(), 2);

        // Verify response shape has expected fields.
        let first = &body.as_array().unwrap()[0];
        assert!(first["request_id"].is_string());
        assert!(first["model"].is_string());
        assert!(first["provider"].is_string());
        assert!(first["prompt_tokens"].is_number());
        assert!(first["completion_tokens"].is_number());
        assert!(first["total_tokens"].is_number());
        assert!(first["created_at"].is_string());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn client_usage_details_returns_404_for_unknown_client(pool: sqlx::PgPool) {
        let app = make_app(make_state(pool));
        let resp = app
            .oneshot(admin_request(
                "GET",
                &format!("/api/clients/{}/usage/details", Uuid::new_v4()),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn create_client_with_budget_fields(pool: sqlx::PgPool) {
        let app = make_app(make_state(pool));
        let resp = app
            .oneshot(admin_request(
                "POST",
                "/api/clients",
                Some(r#"{"name":"budgeted","allowed_models":["*"],"rpm":10,"burst":5,"token_ttl_seconds":900,"token_budget":100000,"budget_period":"monthly"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
                .unwrap();
        assert_eq!(body["token_budget"].as_i64().unwrap(), 100000);
        assert_eq!(body["budget_period"].as_str().unwrap(), "monthly");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn create_client_rejects_mismatched_budget(pool: sqlx::PgPool) {
        let app = make_app(make_state(pool));
        // token_budget without budget_period
        let resp = app
            .oneshot(admin_request(
                "POST",
                "/api/clients",
                Some(r#"{"name":"bad","allowed_models":["*"],"rpm":10,"burst":5,"token_ttl_seconds":900,"token_budget":100000}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn update_budget_and_get_client_shows_budget(pool: sqlx::PgPool) {
        let state = make_state(pool.clone());
        let app = make_app(state);

        // Create a client without budget.
        let resp = app
            .clone()
            .oneshot(admin_request(
                "POST",
                "/api/clients",
                Some(r#"{"name":"patch-test","allowed_models":["*"],"rpm":10,"burst":5,"token_ttl_seconds":900}"#),
            ))
            .await
            .unwrap();
        let created: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
                .unwrap();
        let id = created["id"].as_str().unwrap();

        // PATCH budget.
        let resp = app
            .clone()
            .oneshot(admin_request(
                "PATCH",
                &format!("/api/clients/{id}/budget"),
                Some(r#"{"token_budget":50000,"budget_period":"daily"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
                .unwrap();
        assert_eq!(body["token_budget"].as_i64().unwrap(), 50000);
        assert_eq!(body["budget_period"].as_str().unwrap(), "daily");
        assert!(body["budget_reset_at"].is_string());

        // GET client should also show budget.
        let resp = app
            .oneshot(admin_request("GET", &format!("/api/clients/{id}"), None))
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
                .unwrap();
        assert_eq!(body["token_budget"].as_i64().unwrap(), 50000);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn reactivate_revoked_client(pool: sqlx::PgPool) {
        let state = make_state(pool.clone());
        let app = make_app(state);

        // Create and revoke a client.
        let resp = app
            .clone()
            .oneshot(admin_request(
                "POST",
                "/api/clients",
                Some(r#"{"name":"reactivate-me","allowed_models":["*"],"rpm":10,"burst":5,"token_ttl_seconds":900}"#),
            ))
            .await
            .unwrap();
        let created: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
                .unwrap();
        let id = created["id"].as_str().unwrap();

        app.clone()
            .oneshot(admin_request("DELETE", &format!("/api/clients/{id}"), None))
            .await
            .unwrap();

        // Reactivate.
        let resp = app
            .clone()
            .oneshot(admin_request(
                "POST",
                &format!("/api/clients/{id}/reactivate"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Verify client is active again.
        let uuid: Uuid = id.parse().unwrap();
        let client = crate::db::client_repo::ClientRepository::new(&pool)
            .find_by_id(uuid)
            .await
            .unwrap()
            .unwrap();
        assert!(client.active);
        assert!(client.revoked_at.is_none());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn reactivate_active_client_returns_404(pool: sqlx::PgPool) {
        let state = make_state(pool.clone());
        let app = make_app(state);

        let resp = app
            .clone()
            .oneshot(admin_request(
                "POST",
                "/api/clients",
                Some(r#"{"name":"already-active","allowed_models":["*"],"rpm":10,"burst":5,"token_ttl_seconds":900}"#),
            ))
            .await
            .unwrap();
        let created: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
                .unwrap();
        let id = created["id"].as_str().unwrap();

        let resp = app
            .oneshot(admin_request(
                "POST",
                &format!("/api/clients/{id}/reactivate"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
