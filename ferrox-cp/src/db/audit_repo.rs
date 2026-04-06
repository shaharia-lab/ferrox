use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use uuid::Uuid;

use crate::db::error::RepoError;
use crate::db::models::{AuditEntry, AuditEvent};

/// Optional filters for [`AuditRepository::list`].
#[derive(Debug, Default)]
pub struct AuditFilter {
    /// Return only entries for this client.
    pub client_id: Option<Uuid>,
    /// Return only entries with this event type.
    pub event: Option<AuditEvent>,
    /// Return entries created at or after this timestamp.
    pub since: Option<DateTime<Utc>>,
    /// Maximum number of rows to return (default: 100).
    pub limit: Option<i64>,
    /// Number of rows to skip (for pagination).
    pub offset: Option<i64>,
}

/// Typed repository for the `audit_log` table.
pub struct AuditRepository<'a> {
    db: &'a sqlx::PgPool,
}

impl<'a> AuditRepository<'a> {
    pub fn new(db: &'a sqlx::PgPool) -> Self {
        Self { db }
    }

    /// Append a new audit entry.
    pub async fn record(
        &self,
        client_id: Option<Uuid>,
        event: &AuditEvent,
        metadata: Option<&JsonValue>,
    ) -> Result<(), RepoError> {
        sqlx::query(
            r#"
            INSERT INTO audit_log (client_id, event, metadata)
            VALUES ($1, $2, $3)
            "#,
        )
        .bind(client_id)
        .bind(event.as_str())
        .bind(metadata)
        .execute(self.db)
        .await
        .map_err(RepoError::Database)?;

        Ok(())
    }

    /// Return audit entries matching the given filter, newest first.
    pub async fn list(&self, filter: AuditFilter) -> Result<Vec<AuditEntry>, RepoError> {
        let limit = filter.limit.unwrap_or(100);
        let offset = filter.offset.unwrap_or(0);
        let event_str = filter.event.as_ref().map(|e| e.as_str().to_string());

        let rows = sqlx::query_as::<_, AuditEntry>(
            r#"
            SELECT id, client_id, event, metadata, created_at
            FROM audit_log
            WHERE
                ($1::uuid IS NULL OR client_id = $1)
                AND ($2::text IS NULL OR event = $2)
                AND ($3::timestamptz IS NULL OR created_at >= $3)
            ORDER BY created_at DESC
            LIMIT $4
            OFFSET $5
            "#,
        )
        .bind(filter.client_id)
        .bind(event_str)
        .bind(filter.since)
        .bind(limit)
        .bind(offset)
        .fetch_all(self.db)
        .await
        .map_err(RepoError::Database)?;

        Ok(rows)
    }

    /// Count `token_issued` events for a client since the given timestamp.
    ///
    /// Used by the rate-limit guard in the token-issuance endpoint to enforce
    /// per-client token issuance quotas.
    pub async fn count_tokens_issued(
        &self,
        client_id: Uuid,
        since: DateTime<Utc>,
    ) -> Result<i64, RepoError> {
        let row: (i64,) = sqlx::query_as(
            r#"
            SELECT COUNT(*)
            FROM audit_log
            WHERE client_id = $1
              AND event = 'token_issued'
              AND created_at >= $2
            "#,
        )
        .bind(client_id)
        .bind(since)
        .fetch_one(self.db)
        .await
        .map_err(RepoError::Database)?;

        Ok(row.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::client_repo::ClientRepository;

    async fn create_client(pool: &sqlx::PgPool, name: &str) -> Uuid {
        ClientRepository::new(pool)
            .create(
                name,
                None,
                "pfx00000",
                "hash",
                &["*".to_string()],
                10,
                5,
                300,
                None,
                None,
            )
            .await
            .unwrap()
            .id
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn record_and_list_all(pool: sqlx::PgPool) {
        let audit = AuditRepository::new(&pool);
        let cid = create_client(&pool, "acme").await;

        audit
            .record(Some(cid), &AuditEvent::ClientCreated, None)
            .await
            .expect("record ok");
        audit
            .record(Some(cid), &AuditEvent::TokenIssued, None)
            .await
            .unwrap();

        let entries = audit.list(AuditFilter::default()).await.expect("list ok");
        assert_eq!(entries.len(), 2);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn list_is_ordered_newest_first(pool: sqlx::PgPool) {
        let audit = AuditRepository::new(&pool);
        let cid = create_client(&pool, "order-test").await;

        audit
            .record(Some(cid), &AuditEvent::ClientCreated, None)
            .await
            .unwrap();
        audit
            .record(Some(cid), &AuditEvent::TokenIssued, None)
            .await
            .unwrap();

        let entries = audit.list(AuditFilter::default()).await.unwrap();
        // Newest first: token_issued was recorded second
        assert_eq!(entries[0].event, AuditEvent::TokenIssued);
        assert_eq!(entries[1].event, AuditEvent::ClientCreated);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn list_filters_by_client_id(pool: sqlx::PgPool) {
        let audit = AuditRepository::new(&pool);
        let c1 = create_client(&pool, "c1").await;
        let c2 = create_client(&pool, "c2").await;

        audit
            .record(Some(c1), &AuditEvent::TokenIssued, None)
            .await
            .unwrap();
        audit
            .record(Some(c2), &AuditEvent::TokenIssued, None)
            .await
            .unwrap();

        let entries = audit
            .list(AuditFilter {
                client_id: Some(c1),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].client_id, Some(c1));
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn list_filters_by_event_type(pool: sqlx::PgPool) {
        let audit = AuditRepository::new(&pool);
        let cid = create_client(&pool, "ev-test").await;

        audit
            .record(Some(cid), &AuditEvent::ClientCreated, None)
            .await
            .unwrap();
        audit
            .record(Some(cid), &AuditEvent::TokenIssued, None)
            .await
            .unwrap();

        let entries = audit
            .list(AuditFilter {
                event: Some(AuditEvent::TokenIssued),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event, AuditEvent::TokenIssued);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn list_filters_by_since(pool: sqlx::PgPool) {
        let audit = AuditRepository::new(&pool);
        let cid = create_client(&pool, "ts-test").await;

        audit
            .record(Some(cid), &AuditEvent::KeyRotated, None)
            .await
            .unwrap();

        let future = Utc::now() + chrono::Duration::hours(1);
        let entries = audit
            .list(AuditFilter {
                since: Some(future),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(entries.is_empty());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn list_respects_limit(pool: sqlx::PgPool) {
        let audit = AuditRepository::new(&pool);
        let cid = create_client(&pool, "limit-test").await;

        for _ in 0..5 {
            audit
                .record(Some(cid), &AuditEvent::TokenIssued, None)
                .await
                .unwrap();
        }

        let entries = audit
            .list(AuditFilter {
                limit: Some(3),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(entries.len(), 3);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn list_allows_null_client_id(pool: sqlx::PgPool) {
        let audit = AuditRepository::new(&pool);
        audit
            .record(None, &AuditEvent::KeyRotated, None)
            .await
            .expect("system-level event with no client");

        let entries = audit.list(AuditFilter::default()).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].client_id.is_none());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn record_stores_metadata(pool: sqlx::PgPool) {
        let audit = AuditRepository::new(&pool);
        let cid = create_client(&pool, "meta-test").await;
        let meta = serde_json::json!({"key": "sk-...", "reason": "test"});

        audit
            .record(Some(cid), &AuditEvent::ClientCreated, Some(&meta))
            .await
            .unwrap();

        let entries = audit.list(AuditFilter::default()).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].metadata.is_some());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn count_tokens_issued_counts_only_matching_events(pool: sqlx::PgPool) {
        let audit = AuditRepository::new(&pool);
        let c1 = create_client(&pool, "token-counter").await;
        let c2 = create_client(&pool, "other-client").await;
        let since = Utc::now() - chrono::Duration::seconds(1);

        audit
            .record(Some(c1), &AuditEvent::TokenIssued, None)
            .await
            .unwrap();
        audit
            .record(Some(c1), &AuditEvent::TokenIssued, None)
            .await
            .unwrap();
        audit
            .record(Some(c1), &AuditEvent::ClientCreated, None)
            .await
            .unwrap();
        audit
            .record(Some(c2), &AuditEvent::TokenIssued, None)
            .await
            .unwrap();

        let count = audit
            .count_tokens_issued(c1, since)
            .await
            .expect("count ok");
        assert_eq!(count, 2);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn count_tokens_issued_respects_since_boundary(pool: sqlx::PgPool) {
        let audit = AuditRepository::new(&pool);
        let cid = create_client(&pool, "boundary-test").await;

        audit
            .record(Some(cid), &AuditEvent::TokenIssued, None)
            .await
            .unwrap();

        let future = Utc::now() + chrono::Duration::hours(1);
        let count = audit.count_tokens_issued(cid, future).await.unwrap();
        assert_eq!(count, 0);
    }
}
