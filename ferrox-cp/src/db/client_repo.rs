use uuid::Uuid;

use crate::db::error::RepoError;
use crate::db::models::Client;

/// Typed repository for the `clients` table.
pub struct ClientRepository<'a> {
    db: &'a sqlx::PgPool,
}

impl<'a> ClientRepository<'a> {
    pub fn new(db: &'a sqlx::PgPool) -> Self {
        Self { db }
    }

    /// Insert a new client and return the persisted row.
    ///
    /// `key_prefix` must be the first 8 characters of the raw API key.
    /// `api_key_hash` must be the bcrypt hash of the full raw API key.
    /// The caller is responsible for generating both before calling this method.
    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        &self,
        name: &str,
        description: Option<&str>,
        key_prefix: &str,
        api_key_hash: &str,
        allowed_models: &[String],
        rpm: i32,
        burst: i32,
        token_ttl_seconds: i32,
        token_budget: Option<i64>,
        budget_period: Option<&str>,
    ) -> Result<Client, RepoError> {
        let client = sqlx::query_as::<_, Client>(
            r#"
            INSERT INTO clients
                (name, description, key_prefix, api_key_hash, allowed_models, rpm, burst,
                 token_ttl_seconds, token_budget, budget_period, budget_reset_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                    CASE WHEN $9 IS NOT NULL THEN now() ELSE NULL END)
            RETURNING
                id, name, description, key_prefix, api_key_hash,
                allowed_models, rpm, burst, token_ttl_seconds,
                active, created_at, revoked_at,
                token_budget, budget_period, budget_reset_at
            "#,
        )
        .bind(name)
        .bind(description)
        .bind(key_prefix)
        .bind(api_key_hash)
        .bind(allowed_models)
        .bind(rpm)
        .bind(burst)
        .bind(token_ttl_seconds)
        .bind(token_budget)
        .bind(budget_period)
        .fetch_one(self.db)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                RepoError::Conflict(format!("client name '{}' already exists", name))
            } else {
                RepoError::Database(e)
            }
        })?;

        Ok(client)
    }

    /// Fetch a client by its primary key.
    pub async fn find_by_id(&self, id: Uuid) -> Result<Option<Client>, RepoError> {
        sqlx::query_as::<_, Client>(
            r#"
            SELECT id, name, description, key_prefix, api_key_hash,
                   allowed_models, rpm, burst, token_ttl_seconds,
                   active, created_at, revoked_at,
                token_budget, budget_period, budget_reset_at
            FROM clients
            WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(self.db)
        .await
        .map_err(RepoError::Database)
    }

    /// Return the first active client whose `key_prefix` matches exactly.
    ///
    /// The prefix is the first 8 characters of the raw API key as stored at
    /// creation time.  Because the prefix is plaintext and indexed, this is a
    /// fast O(log n) lookup.  The caller **must** perform a full bcrypt
    /// comparison against `client.api_key_hash` before trusting the result.
    pub async fn find_by_key_prefix(&self, prefix: &str) -> Result<Option<Client>, RepoError> {
        sqlx::query_as::<_, Client>(
            r#"
            SELECT id, name, description, key_prefix, api_key_hash,
                   allowed_models, rpm, burst, token_ttl_seconds,
                   active, created_at, revoked_at,
                token_budget, budget_period, budget_reset_at
            FROM clients
            WHERE active = true
              AND key_prefix = $1
            LIMIT 1
            "#,
        )
        .bind(prefix)
        .fetch_optional(self.db)
        .await
        .map_err(RepoError::Database)
    }

    /// Return a page of clients ordered by creation time (ascending).
    pub async fn list(&self, limit: i64, offset: i64) -> Result<Vec<Client>, RepoError> {
        sqlx::query_as::<_, Client>(
            r#"
            SELECT id, name, description, key_prefix, api_key_hash,
                   allowed_models, rpm, burst, token_ttl_seconds,
                   active, created_at, revoked_at,
                token_budget, budget_period, budget_reset_at
            FROM clients
            ORDER BY created_at ASC
            LIMIT $1 OFFSET $2
            "#,
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(self.db)
        .await
        .map_err(RepoError::Database)
    }

    /// Soft-delete a client by setting `active = false` and stamping `revoked_at`.
    pub async fn revoke(&self, id: Uuid) -> Result<(), RepoError> {
        let rows = sqlx::query(
            r#"
            UPDATE clients
            SET active = false, revoked_at = now()
            WHERE id = $1 AND active = true
            "#,
        )
        .bind(id)
        .execute(self.db)
        .await
        .map_err(RepoError::Database)?
        .rows_affected();

        if rows == 0 {
            Err(RepoError::NotFound(format!("client {}", id)))
        } else {
            Ok(())
        }
    }

    /// Count clients with `active = true`.
    pub async fn count_active(&self) -> Result<i64, RepoError> {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM clients WHERE active = true")
            .fetch_one(self.db)
            .await
            .map_err(RepoError::Database)?;

        Ok(row.0)
    }

    /// Update budget settings for a client.
    pub async fn update_budget(
        &self,
        id: Uuid,
        token_budget: Option<i64>,
        budget_period: Option<&str>,
    ) -> Result<Client, RepoError> {
        // When setting a budget for the first time, initialize budget_reset_at to now.
        let client = sqlx::query_as::<_, Client>(
            r#"
            UPDATE clients
            SET token_budget = $2,
                budget_period = $3,
                budget_reset_at = CASE
                    WHEN $2 IS NOT NULL AND budget_reset_at IS NULL THEN now()
                    WHEN $2 IS NULL THEN NULL
                    ELSE budget_reset_at
                END
            WHERE id = $1
            RETURNING id, name, description, key_prefix, api_key_hash,
                      allowed_models, rpm, burst, token_ttl_seconds,
                      active, created_at, revoked_at,
                      token_budget, budget_period, budget_reset_at
            "#,
        )
        .bind(id)
        .bind(token_budget)
        .bind(budget_period)
        .fetch_optional(self.db)
        .await
        .map_err(RepoError::Database)?
        .ok_or_else(|| RepoError::NotFound(format!("client {}", id)))?;

        Ok(client)
    }

    /// Re-activate a previously revoked client and reset its budget period.
    pub async fn reactivate(&self, id: Uuid) -> Result<(), RepoError> {
        let rows = sqlx::query(
            r#"
            UPDATE clients
            SET active = true, revoked_at = NULL, budget_reset_at = now()
            WHERE id = $1 AND active = false
            "#,
        )
        .bind(id)
        .execute(self.db)
        .await
        .map_err(RepoError::Database)?
        .rows_affected();

        if rows == 0 {
            Err(RepoError::NotFound(format!(
                "client {} not found or already active",
                id
            )))
        } else {
            Ok(())
        }
    }

    /// Find active clients that have a token budget and have exceeded it
    /// based on usage since `budget_reset_at`.
    pub async fn find_over_budget(&self) -> Result<Vec<OverBudgetClient>, RepoError> {
        let rows = sqlx::query_as::<_, OverBudgetClient>(
            r#"
            SELECT c.id, c.name, c.token_budget, COALESCE(SUM(u.total_tokens::bigint), 0)::bigint AS tokens_used
            FROM clients c
            LEFT JOIN usage_log u ON u.client_id = c.id AND u.created_at >= c.budget_reset_at
            WHERE c.active = true
              AND c.token_budget IS NOT NULL
              AND c.budget_reset_at IS NOT NULL
            GROUP BY c.id, c.name, c.token_budget
            HAVING COALESCE(SUM(u.total_tokens::bigint), 0) >= c.token_budget
            "#,
        )
        .fetch_all(self.db)
        .await
        .map_err(RepoError::Database)?;

        Ok(rows)
    }

    /// Reset `budget_reset_at` to the next period boundary for clients
    /// whose current period has expired.
    pub async fn reset_expired_budgets(&self) -> Result<u64, RepoError> {
        let result = sqlx::query(
            r#"
            UPDATE clients
            SET budget_reset_at = CASE budget_period
                WHEN 'daily' THEN budget_reset_at + INTERVAL '1 day'
                WHEN 'monthly' THEN budget_reset_at + INTERVAL '1 month'
                ELSE budget_reset_at
            END
            WHERE active = true
              AND token_budget IS NOT NULL
              AND budget_reset_at IS NOT NULL
              AND budget_period IS NOT NULL
              AND (
                  (budget_period = 'daily' AND budget_reset_at + INTERVAL '1 day' <= now())
                  OR (budget_period = 'monthly' AND budget_reset_at + INTERVAL '1 month' <= now())
              )
            "#,
        )
        .execute(self.db)
        .await
        .map_err(RepoError::Database)?;

        Ok(result.rows_affected())
    }
}

/// Result of the over-budget query.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OverBudgetClient {
    pub id: Uuid,
    pub name: String,
    pub token_budget: Option<i64>,
    pub tokens_used: i64,
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    if let sqlx::Error::Database(db_err) = e {
        return db_err.code().as_deref() == Some("23505");
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn create_and_find_by_id(pool: sqlx::PgPool) {
        let repo = ClientRepository::new(&pool);
        let client = repo
            .create(
                "acme",
                Some("ACME Corp"),
                "sk_test",
                "hash_placeholder",
                &["gpt-4".to_string()],
                100,
                10,
                600,
                None,
                None,
            )
            .await
            .expect("create should succeed");

        assert_eq!(client.name, "acme");
        assert_eq!(client.description.as_deref(), Some("ACME Corp"));
        assert_eq!(client.key_prefix, "sk_test");
        assert!(client.active);
        assert!(client.revoked_at.is_none());

        let found = repo.find_by_id(client.id).await.expect("query ok");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "acme");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn create_stores_allowed_models(pool: sqlx::PgPool) {
        let repo = ClientRepository::new(&pool);
        let models = vec!["gpt-4".to_string(), "claude-3".to_string()];
        let client = repo
            .create(
                "m-test", None, "pfx12345", "h", &models, 10, 5, 300, None, None,
            )
            .await
            .unwrap();
        assert_eq!(client.allowed_models, models);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn create_duplicate_name_returns_conflict(pool: sqlx::PgPool) {
        let repo = ClientRepository::new(&pool);
        repo.create(
            "dup",
            None,
            "pfx00001",
            "h1",
            &["*".to_string()],
            10,
            5,
            300,
            None,
            None,
        )
        .await
        .expect("first insert ok");
        let err = repo
            .create(
                "dup",
                None,
                "pfx00002",
                "h2",
                &["*".to_string()],
                10,
                5,
                300,
                None,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RepoError::Conflict(_)));
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn find_by_id_returns_none_for_unknown(pool: sqlx::PgPool) {
        let repo = ClientRepository::new(&pool);
        let result = repo.find_by_id(Uuid::new_v4()).await.expect("query ok");
        assert!(result.is_none());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn find_by_key_prefix_returns_active_client(pool: sqlx::PgPool) {
        let repo = ClientRepository::new(&pool);
        repo.create(
            "pfx-test",
            None,
            "abcd1234",
            "bcrypt_hash_placeholder",
            &["*".to_string()],
            10,
            5,
            300,
            None,
            None,
        )
        .await
        .unwrap();

        // Exact prefix match returns the client.
        let found = repo.find_by_key_prefix("abcd1234").await.expect("query ok");
        assert!(found.is_some());
        assert_eq!(found.unwrap().key_prefix, "abcd1234");

        // Wrong prefix returns nothing.
        let not_found = repo.find_by_key_prefix("xxxxxxxx").await.expect("query ok");
        assert!(not_found.is_none());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn find_by_key_prefix_excludes_revoked_clients(pool: sqlx::PgPool) {
        let repo = ClientRepository::new(&pool);
        let client = repo
            .create(
                "revoked-pfx",
                None,
                "rev12345",
                "h",
                &["*".to_string()],
                10,
                5,
                300,
                None,
                None,
            )
            .await
            .unwrap();

        repo.revoke(client.id).await.unwrap();

        // Prefix of a revoked client must not be returned.
        let result = repo.find_by_key_prefix("rev12345").await.expect("query ok");
        assert!(result.is_none());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn list_returns_all_clients(pool: sqlx::PgPool) {
        let repo = ClientRepository::new(&pool);
        for i in 0..3_u32 {
            repo.create(
                &format!("client-{}", i),
                None,
                &format!("pfx0000{}", i),
                &format!("hash-{}", i),
                &["*".to_string()],
                10,
                5,
                300,
                None,
                None,
            )
            .await
            .unwrap();
        }
        let clients = repo.list(10, 0).await.expect("list ok");
        assert_eq!(clients.len(), 3);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn list_respects_limit_and_offset(pool: sqlx::PgPool) {
        let repo = ClientRepository::new(&pool);
        for i in 0..5_u32 {
            repo.create(
                &format!("c-{}", i),
                None,
                &format!("pfx0000{}", i),
                &format!("h-{}", i),
                &["*".to_string()],
                10,
                5,
                300,
                None,
                None,
            )
            .await
            .unwrap();
        }
        let page1 = repo.list(2, 0).await.expect("page 1 ok");
        let page2 = repo.list(2, 2).await.expect("page 2 ok");
        assert_eq!(page1.len(), 2);
        assert_eq!(page2.len(), 2);
        assert_ne!(page1[0].id, page2[0].id);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn revoke_sets_inactive_and_stamps_revoked_at(pool: sqlx::PgPool) {
        let repo = ClientRepository::new(&pool);
        let client = repo
            .create(
                "to-revoke",
                None,
                "rev00001",
                "h",
                &["*".to_string()],
                10,
                5,
                300,
                None,
                None,
            )
            .await
            .unwrap();

        repo.revoke(client.id).await.expect("revoke ok");

        let updated = repo.find_by_id(client.id).await.unwrap().unwrap();
        assert!(!updated.active);
        assert!(updated.revoked_at.is_some());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn revoke_unknown_id_returns_not_found(pool: sqlx::PgPool) {
        let repo = ClientRepository::new(&pool);
        let err = repo.revoke(Uuid::new_v4()).await.unwrap_err();
        assert!(matches!(err, RepoError::NotFound(_)));
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn count_active_tracks_creates_and_revocations(pool: sqlx::PgPool) {
        let repo = ClientRepository::new(&pool);
        assert_eq!(repo.count_active().await.unwrap(), 0);

        let a = repo
            .create(
                "a",
                None,
                "pfx_a_01",
                "h",
                &["*".to_string()],
                10,
                5,
                300,
                None,
                None,
            )
            .await
            .unwrap();
        repo.create(
            "b",
            None,
            "pfx_b_01",
            "h2",
            &["*".to_string()],
            10,
            5,
            300,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(repo.count_active().await.unwrap(), 2);
        repo.revoke(a.id).await.unwrap();
        assert_eq!(repo.count_active().await.unwrap(), 1);
    }
}
