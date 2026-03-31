use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;

use crate::state::CpState;

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
}

/// `GET /healthz` — liveness + DB connectivity probe.
///
/// Returns `200 {"status":"ok"}` when the database is reachable.
/// Returns `503 {"status":"unavailable"}` if the `SELECT 1` fails.
pub async fn health_handler(State(state): State<CpState>) -> (StatusCode, Json<HealthResponse>) {
    match sqlx::query("SELECT 1").execute(&state.db).await {
        Ok(_) => (StatusCode::OK, Json(HealthResponse { status: "ok" })),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(HealthResponse {
                status: "unavailable",
            }),
        ),
    }
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
    async fn health_returns_200_when_db_reachable(pool: sqlx::PgPool) {
        let state = make_state(pool);
        let app = Router::new()
            .route("/healthz", get(health_handler))
            .with_state(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);

        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1024).await.unwrap())
                .unwrap();
        assert_eq!(body["status"], "ok");
    }
}
