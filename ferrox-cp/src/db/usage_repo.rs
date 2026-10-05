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
        let cache_read_tokens: Vec<Option<i32>> =
            records.iter().map(|r| r.cache_read_tokens).collect();
        let cache_write_tokens: Vec<Option<i32>> =
            records.iter().map(|r| r.cache_write_tokens).collect();
        let latency_ms: Vec<Option<i32>> = records.iter().map(|r| r.latency_ms).collect();
        let requested_models: Vec<Option<&str>> = records
            .iter()
            .map(|r| r.requested_model.as_deref())
            .collect();
        let routing_reasons: Vec<Option<&str>> = records
            .iter()
            .map(|r| r.routing_reason.as_deref())
            .collect();
        let classifier_confidences: Vec<Option<f64>> =
            records.iter().map(|r| r.classifier_confidence).collect();
        let classifier_latency_ms: Vec<Option<i32>> =
            records.iter().map(|r| r.classifier_latency_ms).collect();
        let classifier_input_tokens: Vec<Option<i32>> =
            records.iter().map(|r| r.classifier_input_tokens).collect();
        let classifier_models: Vec<Option<&str>> = records
            .iter()
            .map(|r| r.classifier_model.as_deref())
            .collect();

        sqlx::query(
            r#"
            INSERT INTO usage_log
                (client_id, request_id, model, provider, prompt_tokens, completion_tokens, total_tokens,
                 cache_read_tokens, cache_write_tokens, latency_ms,
                 requested_model, routing_reason, classifier_confidence, classifier_latency_ms,
                 classifier_input_tokens, classifier_model)
            SELECT * FROM UNNEST(
                $1::uuid[], $2::text[], $3::text[], $4::text[],
                $5::int[], $6::int[], $7::int[], $8::int[], $9::int[], $10::int[],
                $11::text[], $12::text[], $13::float8[], $14::int[], $15::int[], $16::text[]
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
        .bind(&cache_read_tokens)
        .bind(&cache_write_tokens)
        .bind(&latency_ms)
        .bind(&requested_models)
        .bind(&routing_reasons)
        .bind(&classifier_confidences)
        .bind(&classifier_latency_ms)
        .bind(&classifier_input_tokens)
        .bind(&classifier_models)
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
                   cache_read_tokens, cache_write_tokens,
                   latency_ms, created_at,
                   requested_model, routing_reason, classifier_confidence,
                   classifier_latency_ms, classifier_input_tokens, classifier_model
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
    /// `None` writes SQL `NULL`, meaning "not recorded" rather than "zero".
    pub cache_read_tokens: Option<i32>,
    pub cache_write_tokens: Option<i32>,
    pub latency_ms: Option<i32>,
    /// The routing decision of a request to a classified alias. All `None`
    /// (SQL `NULL`) for a statically routed one.
    pub requested_model: Option<String>,
    pub routing_reason: Option<String>,
    pub classifier_confidence: Option<f64>,
    pub classifier_latency_ms: Option<i32>,
    pub classifier_input_tokens: Option<i32>,
    pub classifier_model: Option<String>,
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
            cache_read_tokens: None,
            cache_write_tokens: None,
            latency_ms: Some(150),
            requested_model: None,
            routing_reason: None,
            classifier_confidence: None,
            classifier_latency_ms: None,
            classifier_input_tokens: None,
            classifier_model: None,
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
    async fn insert_batch_round_trips_cache_tokens(pool: sqlx::PgPool) {
        let cid = create_client(&pool, "cache-tokens").await;
        let repo = UsageRepository::new(&pool);

        let mut cached = sample_insert(cid, "claude-sonnet", 47, 2);
        cached.cache_read_tokens = Some(3968);
        cached.cache_write_tokens = Some(100);
        // A provider that reports no cache usage records an explicit zero.
        let mut uncached = sample_insert(cid, "claude-sonnet", 100, 50);
        uncached.cache_read_tokens = Some(0);
        uncached.cache_write_tokens = Some(0);
        // A gateway predating cache accounting records NULL.
        let legacy = sample_insert(cid, "claude-sonnet", 10, 5);

        repo.insert_batch(&[cached, uncached, legacy])
            .await
            .expect("insert ok");

        let rows = repo
            .list(UsageFilter {
                client_id: cid,
                ..Default::default()
            })
            .await
            .expect("list ok");
        assert_eq!(rows.len(), 3);

        let by_prompt = |p: i32| {
            rows.iter()
                .find(|r| r.prompt_tokens == p)
                .unwrap_or_else(|| panic!("row with prompt_tokens={p}"))
        };
        assert_eq!(by_prompt(47).cache_read_tokens, Some(3968));
        assert_eq!(by_prompt(47).cache_write_tokens, Some(100));
        assert_eq!(by_prompt(100).cache_read_tokens, Some(0));
        assert_eq!(
            by_prompt(10).cache_read_tokens,
            None,
            "NULL must stay distinct from a recorded 0"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn insert_batch_round_trips_the_routing_decision(pool: sqlx::PgPool) {
        let cid = create_client(&pool, "classifier-columns").await;
        let repo = UsageRepository::new(&pool);

        // Served by `fast` for a request to the classified alias `auto`.
        let mut classified = sample_insert(cid, "fast", 47, 2);
        classified.requested_model = Some("auto".to_string());
        classified.routing_reason = Some("classified".to_string());
        classified.classifier_confidence = Some(0.9);
        classified.classifier_latency_ms = Some(84);
        classified.classifier_input_tokens = Some(31);
        classified.classifier_model = Some("jev-1".to_string());
        // The classifier timed out: a decision, but no answer to report.
        let mut timed_out = sample_insert(cid, "smart", 100, 50);
        timed_out.requested_model = Some("auto".to_string());
        timed_out.routing_reason = Some("timeout".to_string());
        timed_out.classifier_latency_ms = Some(50);
        // A statically routed request.
        let stat = sample_insert(cid, "fast", 10, 5);

        repo.insert_batch(&[classified, timed_out, stat])
            .await
            .expect("insert ok");

        let rows = repo
            .list(UsageFilter {
                client_id: cid,
                ..Default::default()
            })
            .await
            .expect("list ok");
        assert_eq!(rows.len(), 3);
        let by_prompt = |p: i32| {
            rows.iter()
                .find(|r| r.prompt_tokens == p)
                .unwrap_or_else(|| panic!("row with prompt_tokens={p}"))
        };

        let row = by_prompt(47);
        assert_eq!(row.model, "fast");
        assert_eq!(row.requested_model.as_deref(), Some("auto"));
        assert_eq!(row.routing_reason.as_deref(), Some("classified"));
        assert_eq!(row.classifier_confidence, Some(0.9));
        assert_eq!(row.classifier_latency_ms, Some(84));
        assert_eq!(row.classifier_input_tokens, Some(31));
        assert_eq!(row.classifier_model.as_deref(), Some("jev-1"));

        let row = by_prompt(100);
        assert_eq!(row.routing_reason.as_deref(), Some("timeout"));
        assert_eq!(row.classifier_latency_ms, Some(50));
        assert_eq!(row.classifier_confidence, None);
        assert_eq!(row.classifier_input_tokens, None);
        assert_eq!(row.classifier_model, None);

        // NULL in every column: not a request to a classified alias.
        let row = by_prompt(10);
        assert_eq!(row.requested_model, None);
        assert_eq!(row.routing_reason, None);
        assert_eq!(row.classifier_confidence, None);
        assert_eq!(row.classifier_latency_ms, None);
        assert_eq!(row.classifier_input_tokens, None);
        assert_eq!(row.classifier_model, None);
    }

    /// The classifier migration applies on a database that already holds
    /// usage at the previous schema version, and leaves those rows `NULL`.
    #[sqlx::test(migrations = false)]
    async fn classifier_migration_applies_to_an_existing_database(pool: sqlx::PgPool) {
        const CLASSIFIER_MIGRATION: i64 = 20240005000000;
        let (before, after): (Vec<_>, Vec<_>) = crate::MIGRATOR
            .iter()
            .partition(|m| m.version < CLASSIFIER_MIGRATION);
        assert_eq!(after.first().map(|m| m.version), Some(CLASSIFIER_MIGRATION));

        for migration in before {
            sqlx::raw_sql(&migration.sql)
                .execute(&pool)
                .await
                .expect("earlier migration applies");
        }
        let cid = create_client(&pool, "pre-classifier").await;
        sqlx::query(
            "INSERT INTO usage_log (client_id, request_id, model, provider, prompt_tokens, \
             completion_tokens, total_tokens) VALUES ($1, 'req-old', 'fast', 'openai', 10, 5, 15)",
        )
        .bind(cid)
        .execute(&pool)
        .await
        .expect("row at the previous schema version");

        for migration in after {
            sqlx::raw_sql(&migration.sql)
                .execute(&pool)
                .await
                .expect("classifier migration applies");
        }

        let repo = UsageRepository::new(&pool);
        repo.insert_batch(&[sample_insert(cid, "fast", 20, 10)])
            .await
            .expect("insert after the migration");
        let rows = repo
            .list(UsageFilter {
                client_id: cid,
                ..Default::default()
            })
            .await
            .expect("list ok");
        assert_eq!(rows.len(), 2);
        let old = rows.iter().find(|r| r.request_id == "req-old").unwrap();
        assert_eq!(old.requested_model, None);
        assert_eq!(old.routing_reason, None);
        assert_eq!(old.classifier_confidence, None);
    }

    /// Decided behaviour for a gateway running against an unmigrated database:
    /// the insert **fails loudly** rather than silently dropping the cache
    /// columns. The gateway's flush logs the error and drops that batch, so the
    /// operator sees it; the fix is to deploy the control plane (which applies
    /// migrations at startup) before the gateway.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn insert_batch_fails_loudly_on_unmigrated_schema(pool: sqlx::PgPool) {
        sqlx::query(
            "ALTER TABLE usage_log DROP COLUMN cache_read_tokens, DROP COLUMN cache_write_tokens",
        )
        .execute(&pool)
        .await
        .expect("simulate pre-migration schema");

        let cid = create_client(&pool, "unmigrated").await;
        let repo = UsageRepository::new(&pool);

        let err = repo
            .insert_batch(&[sample_insert(cid, "gpt-4", 100, 50)])
            .await
            .expect_err("insert must fail, not silently succeed");
        assert!(
            matches!(err, RepoError::Database(_)),
            "expected a database error naming the missing column, got {err:?}"
        );
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
