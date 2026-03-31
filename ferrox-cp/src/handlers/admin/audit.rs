use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use tracing::error;
use uuid::Uuid;

use crate::db::audit_repo::{AuditFilter, AuditRepository};
use crate::db::models::AuditEvent;
use crate::state::CpState;

#[derive(Debug, Deserialize)]
pub struct AuditQueryParams {
    pub client_id: Option<Uuid>,
    pub event: Option<String>,
    pub since: Option<DateTime<Utc>>,
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

fn default_limit() -> i64 {
    100
}

/// `GET /api/audit`
///
/// Lists audit log entries with optional filters.
/// Query parameters: `client_id`, `event`, `since`, `limit`, `offset`.
pub async fn list_audit(
    State(state): State<CpState>,
    Query(params): Query<AuditQueryParams>,
) -> Result<Json<Vec<crate::db::models::AuditEntry>>, (StatusCode, Json<serde_json::Value>)> {
    let event = params.event.as_deref().map(|s| match s {
        "token_issued" => AuditEvent::TokenIssued,
        "client_created" => AuditEvent::ClientCreated,
        "client_revoked" => AuditEvent::ClientRevoked,
        "key_rotated" => AuditEvent::KeyRotated,
        other => AuditEvent::Other(other.to_string()),
    });

    let repo = AuditRepository::new(&state.db);
    let entries = repo
        .list(AuditFilter {
            client_id: params.client_id,
            event,
            since: params.since,
            limit: Some(params.limit),
            offset: Some(params.offset),
        })
        .await
        .map_err(|e| {
            error!(error = %e, "db error listing audit entries");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal_error", "message": "database error"})),
            )
        })?;

    Ok(Json(entries))
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
    use crate::middleware::admin_auth::require_admin_key;

    const ADMIN_KEY: &str = "test-admin-key-audit";

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

    fn make_app(state: CpState) -> Router {
        let admin_routes = Router::new().route("/api/audit", get(list_audit)).layer(
            axum::middleware::from_fn_with_state(state.clone(), require_admin_key),
        );
        Router::new().merge(admin_routes).with_state(state)
    }

    fn admin_req(uri: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(uri)
            .header("Authorization", format!("Bearer {ADMIN_KEY}"))
            .body(Body::empty())
            .unwrap()
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn list_audit_returns_entries(pool: sqlx::PgPool) {
        let audit = AuditRepository::new(&pool);
        audit
            .record(None, &AuditEvent::KeyRotated, None)
            .await
            .unwrap();
        audit
            .record(None, &AuditEvent::TokenIssued, None)
            .await
            .unwrap();

        let app = make_app(make_state(pool));
        let resp = app.oneshot(admin_req("/api/audit")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
                .unwrap();
        assert_eq!(body.as_array().unwrap().len(), 2);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn list_audit_filters_by_event(pool: sqlx::PgPool) {
        let audit = AuditRepository::new(&pool);
        audit
            .record(None, &AuditEvent::KeyRotated, None)
            .await
            .unwrap();
        audit
            .record(None, &AuditEvent::TokenIssued, None)
            .await
            .unwrap();

        let app = make_app(make_state(pool));
        let resp = app
            .oneshot(admin_req("/api/audit?event=key_rotated"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
                .unwrap();
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["event"], "key_rotated");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn list_audit_pagination(pool: sqlx::PgPool) {
        let audit = AuditRepository::new(&pool);
        for _ in 0..5 {
            audit
                .record(None, &AuditEvent::TokenIssued, None)
                .await
                .unwrap();
        }

        let app = make_app(make_state(pool));
        let resp = app
            .oneshot(admin_req("/api/audit?limit=2&offset=0"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
                .unwrap();
        assert_eq!(body.as_array().unwrap().len(), 2);
    }
}
