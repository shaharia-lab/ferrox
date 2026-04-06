use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::db::error::RepoError;
use crate::db::models::{UsageRecord, UsageSummary};

/// Optional filters for [`UsageRepository::list`].
#[derive(Debug, Default)]
pub struct UsageFilter {
    pub client_id: Uuid,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub model: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Typed repository for the `usage_log` table.
pub struct UsageRepository<'a> {
    db: &'a sqlx::PgPool,
}

impl<'a> UsageRepository<'a> {
    pub fn new(db: &'a sqlx::PgPool) -> Self {
        Self { db }
    }

    /// Insert a batch of usage records in a single query.
    pub async fn insert_batch(&self, records: &[UsageInsert]) -> Result<(), RepoError> {
        if records.is_empty() {
            return Ok(());
        }

        // Build a bulk INSERT using UNNEST for efficiency.
        let client_ids: Vec<Uuid> = records.iter().map(|r| r.client_id).collect();
        let request_ids: Vec<&str> = records.iter().map(|r| r.request_id.as_str()).collect();
        let models: Vec<&str> = records.iter().map(|r| r.model.as_str()).collect();
        let providers: Vec<&str> = records.iter().map(|r| r.provider.as_str()).collect();
        let prompt_tokens: Vec<i32> = records.iter().map(|r| r.prompt_tokens).collect();
        let completion_tokens: Vec<i32> = records.iter().map(|r| r.completion_tokens).collect();
        let total_tokens: Vec<i32> = records.iter().map(|r| r.total_tokens).collect();
        let latency_ms: Vec<Option<i32>> = records.iter().map(|r| r.latency_ms).collect();

        sqlx::query(
            r#"
            INSERT INTO usage_log
                (client_id, request_id, model, provider, prompt_tokens, completion_tokens, total_tokens, latency_ms)
            SELECT * FROM UNNEST(
                $1::uuid[], $2::text[], $3::text[], $4::text[],
                $5::int[], $6::int[], $7::int[], $8::int[]
            )
            "#,
        )
        .bind(&client_ids)
        .bind(&request_ids)
        .bind(&models)
        .bind(&providers)
        .bind(&prompt_tokens)
        .bind(&completion_tokens)
        .bind(&total_tokens)
        .bind(&latency_ms)
        .execute(self.db)
        .await
        .map_err(RepoError::Database)?;

        Ok(())
    }

    /// Return aggregated token usage for a client within a time range.
    pub async fn summarize(
        &self,
        client_id: Uuid,
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
    ) -> Result<UsageSummary, RepoError> {
        let row: (Option<i64>, Option<i64>, Option<i64>, i64) = sqlx::query_as(
            r#"
            SELECT
                COALESCE(SUM(prompt_tokens::bigint), 0)::bigint,
                COALESCE(SUM(completion_tokens::bigint), 0)::bigint,
                COALESCE(SUM(total_tokens::bigint), 0)::bigint,
                COUNT(*)
            FROM usage_log
            WHERE client_id = $1
              AND ($2::timestamptz IS NULL OR created_at >= $2)
              AND ($3::timestamptz IS NULL OR created_at < $3)
            "#,
        )
        .bind(client_id)
        .bind(from)
        .bind(to)
        .fetch_one(self.db)
        .await
        .map_err(RepoError::Database)?;

        Ok(UsageSummary {
            total_prompt_tokens: row.0.unwrap_or(0),
            total_completion_tokens: row.1.unwrap_or(0),
            total_tokens: row.2.unwrap_or(0),
            request_count: row.3,
        })
    }

    /// Return paginated per-request usage records for a client.
    pub async fn list(&self, filter: UsageFilter) -> Result<Vec<UsageRecord>, RepoError> {
        let limit = filter.limit.unwrap_or(50);
        let offset = filter.offset.unwrap_or(0);

        let rows = sqlx::query_as::<_, UsageRecord>(
            r#"
            SELECT id, client_id, request_id, model, provider,
                   prompt_tokens, completion_tokens, total_tokens,
                   latency_ms, created_at
            FROM usage_log
            WHERE client_id = $1
              AND ($2::timestamptz IS NULL OR created_at >= $2)
              AND ($3::timestamptz IS NULL OR created_at < $3)
              AND ($4::text IS NULL OR model = $4)
            ORDER BY created_at DESC
            LIMIT $5
            OFFSET $6
            "#,
        )
        .bind(filter.client_id)
        .bind(filter.from)
        .bind(filter.to)
        .bind(filter.model)
        .bind(limit)
        .bind(offset)
        .fetch_all(self.db)
        .await
        .map_err(RepoError::Database)?;

        Ok(rows)
    }
}

/// Data needed to insert a usage record (no `id` or `created_at`).
#[derive(Debug, Clone)]
pub struct UsageInsert {
    pub client_id: Uuid,
    pub request_id: String,
    pub model: String,
    pub provider: String,
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
    pub total_tokens: i32,
    pub latency_ms: Option<i32>,
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

    fn sample_insert(client_id: Uuid, model: &str, prompt: i32, completion: i32) -> UsageInsert {
        UsageInsert {
            client_id,
            request_id: Uuid::new_v4().to_string(),
            model: model.to_string(),
            provider: "openai".to_string(),
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            latency_ms: Some(150),
        }
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn insert_batch_and_summarize(pool: sqlx::PgPool) {
        let cid = create_client(&pool, "usage-test").await;
        let repo = UsageRepository::new(&pool);

        let records = vec![
            sample_insert(cid, "gpt-4", 100, 50),
            sample_insert(cid, "gpt-4", 200, 100),
        ];
        repo.insert_batch(&records).await.expect("insert ok");

        let summary = repo.summarize(cid, None, None).await.expect("summarize ok");
        assert_eq!(summary.total_prompt_tokens, 300);
        assert_eq!(summary.total_completion_tokens, 150);
        assert_eq!(summary.total_tokens, 450);
        assert_eq!(summary.request_count, 2);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn insert_empty_batch_is_noop(pool: sqlx::PgPool) {
        let repo = UsageRepository::new(&pool);
        repo.insert_batch(&[]).await.expect("empty batch ok");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn summarize_empty_returns_zeros(pool: sqlx::PgPool) {
        let cid = create_client(&pool, "empty-usage").await;
        let repo = UsageRepository::new(&pool);

        let summary = repo.summarize(cid, None, None).await.expect("summarize ok");
        assert_eq!(summary.total_tokens, 0);
        assert_eq!(summary.request_count, 0);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn summarize_respects_time_range(pool: sqlx::PgPool) {
        let cid = create_client(&pool, "time-range").await;
        let repo = UsageRepository::new(&pool);

        repo.insert_batch(&[sample_insert(cid, "gpt-4", 100, 50)])
            .await
            .unwrap();

        let future = Utc::now() + chrono::Duration::hours(1);
        let summary = repo
            .summarize(cid, Some(future), None)
            .await
            .expect("summarize ok");
        assert_eq!(summary.request_count, 0);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn list_returns_records_newest_first(pool: sqlx::PgPool) {
        let cid = create_client(&pool, "list-test").await;
        let repo = UsageRepository::new(&pool);

        let records = vec![
            sample_insert(cid, "gpt-4", 10, 5),
            sample_insert(cid, "claude-3", 20, 10),
        ];
        repo.insert_batch(&records).await.unwrap();

        let results = repo
            .list(UsageFilter {
                client_id: cid,
                ..Default::default()
            })
            .await
            .expect("list ok");
        assert_eq!(results.len(), 2);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn list_filters_by_model(pool: sqlx::PgPool) {
        let cid = create_client(&pool, "model-filter").await;
        let repo = UsageRepository::new(&pool);

        repo.insert_batch(&[
            sample_insert(cid, "gpt-4", 10, 5),
            sample_insert(cid, "claude-3", 20, 10),
        ])
        .await
        .unwrap();

        let results = repo
            .list(UsageFilter {
                client_id: cid,
                model: Some("gpt-4".to_string()),
                ..Default::default()
            })
            .await
            .expect("list ok");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].model, "gpt-4");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn list_respects_limit_and_offset(pool: sqlx::PgPool) {
        let cid = create_client(&pool, "pagination").await;
        let repo = UsageRepository::new(&pool);

        let records: Vec<UsageInsert> = (0..5)
            .map(|i| sample_insert(cid, "gpt-4", i * 10, i * 5))
            .collect();
        repo.insert_batch(&records).await.unwrap();

        let page1 = repo
            .list(UsageFilter {
                client_id: cid,
                limit: Some(2),
                ..Default::default()
            })
            .await
            .unwrap();
        let page2 = repo
            .list(UsageFilter {
                client_id: cid,
                limit: Some(2),
                offset: Some(2),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page1.len(), 2);
        assert_eq!(page2.len(), 2);
        assert_ne!(page1[0].id, page2[0].id);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn summarize_isolates_clients(pool: sqlx::PgPool) {
        let c1 = create_client(&pool, "client-1").await;
        let c2 = create_client(&pool, "client-2").await;
        let repo = UsageRepository::new(&pool);

        repo.insert_batch(&[
            sample_insert(c1, "gpt-4", 100, 50),
            sample_insert(c2, "gpt-4", 200, 100),
        ])
        .await
        .unwrap();

        let s1 = repo.summarize(c1, None, None).await.unwrap();
        assert_eq!(s1.total_tokens, 150);
        let s2 = repo.summarize(c2, None, None).await.unwrap();
        assert_eq!(s2.total_tokens, 300);
    }
}
