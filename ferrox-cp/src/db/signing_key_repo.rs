use chrono::{DateTime, Utc};

use crate::db::error::RepoError;
use crate::db::models::SigningKey;

/// Typed repository for the `signing_keys` table.
pub struct SigningKeyRepository<'a> {
    db: &'a sqlx::PgPool,
}

impl<'a> SigningKeyRepository<'a> {
    pub fn new(db: &'a sqlx::PgPool) -> Self {
        Self { db }
    }

    /// Persist a new signing keypair and return the stored row.
    ///
    /// `private_key_encrypted` must already be AES-256-GCM encrypted by the
    /// crypto layer before being passed here.  `public_key_der` is the
    /// DER-encoded SubjectPublicKeyInfo bytes.
    pub async fn create(
        &self,
        kid: &str,
        private_key_encrypted: &[u8],
        public_key_der: &[u8],
    ) -> Result<SigningKey, RepoError> {
        let key = sqlx::query_as::<_, SigningKey>(
            r#"
            INSERT INTO signing_keys (kid, private_key, public_key)
            VALUES ($1, $2, $3)
            RETURNING kid, algorithm, private_key, public_key, active, created_at, retired_at
            "#,
        )
        .bind(kid)
        .bind(private_key_encrypted)
        .bind(public_key_der)
        .fetch_one(self.db)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                RepoError::Conflict(format!("signing key '{}' already exists", kid))
            } else {
                RepoError::Database(e)
            }
        })?;

        Ok(key)
    }

    /// Return all active signing keys (used to build the JWKS endpoint).
    pub async fn get_active(&self) -> Result<Vec<SigningKey>, RepoError> {
        sqlx::query_as::<_, SigningKey>(
            r#"
            SELECT kid, algorithm, private_key, public_key, active, created_at, retired_at
            FROM signing_keys
            WHERE active = true
            ORDER BY created_at ASC
            "#,
        )
        .fetch_all(self.db)
        .await
        .map_err(RepoError::Database)
    }

    /// Return the most recently created active key — used for signing new tokens.
    pub async fn get_newest_active(&self) -> Result<Option<SigningKey>, RepoError> {
        sqlx::query_as::<_, SigningKey>(
            r#"
            SELECT kid, algorithm, private_key, public_key, active, created_at, retired_at
            FROM signing_keys
            WHERE active = true
            ORDER BY created_at DESC
            LIMIT 1
            "#,
        )
        .fetch_optional(self.db)
        .await
        .map_err(RepoError::Database)
    }

    /// Return all signing keys (active and retired), ordered newest first.
    ///
    /// Used by the admin API to list all keys for inspection.
    pub async fn get_all(&self) -> Result<Vec<SigningKey>, RepoError> {
        sqlx::query_as::<_, SigningKey>(
            r#"
            SELECT kid, algorithm, private_key, public_key, active, created_at, retired_at
            FROM signing_keys
            ORDER BY created_at DESC
            "#,
        )
        .fetch_all(self.db)
        .await
        .map_err(RepoError::Database)
    }

    /// Schedule a key for retirement at a future timestamp without immediately
    /// deactivating it.  The key remains in the JWKS until the background task
    /// calls [`retire_expired`] after `retire_at` passes, preserving the overlap
    /// window so in-flight tokens stay verifiable.
    pub async fn schedule_retirement(
        &self,
        kid: &str,
        retire_at: DateTime<Utc>,
    ) -> Result<(), RepoError> {
        let rows = sqlx::query(
            r#"
            UPDATE signing_keys
            SET retired_at = $1
            WHERE kid = $2 AND active = true AND retired_at IS NULL
            "#,
        )
        .bind(retire_at)
        .bind(kid)
        .execute(self.db)
        .await
        .map_err(RepoError::Database)?
        .rows_affected();

        if rows == 0 {
            Err(RepoError::NotFound(format!("active signing key '{}'", kid)))
        } else {
            Ok(())
        }
    }

    /// Deactivate all keys whose scheduled `retired_at` timestamp has passed.
    ///
    /// Called by the background key-retirement task every minute.
    /// Returns the number of keys that were deactivated.
    pub async fn retire_expired(&self) -> Result<u64, RepoError> {
        let n = sqlx::query(
            r#"
            UPDATE signing_keys
            SET active = false
            WHERE active = true AND retired_at IS NOT NULL AND retired_at <= now()
            "#,
        )
        .execute(self.db)
        .await
        .map_err(RepoError::Database)?
        .rows_affected();

        Ok(n)
    }

    /// Retire a signing key (sets `active = false`, stamps `retired_at`).
    ///
    /// Retired keys are no longer used for signing but remain in the database
    /// so JWKS consumers can verify tokens that were issued before rotation.
    pub async fn retire(&self, kid: &str) -> Result<(), RepoError> {
        let rows = sqlx::query(
            r#"
            UPDATE signing_keys
            SET active = false, retired_at = now()
            WHERE kid = $1 AND active = true
            "#,
        )
        .bind(kid)
        .execute(self.db)
        .await
        .map_err(RepoError::Database)?
        .rows_affected();

        if rows == 0 {
            Err(RepoError::NotFound(format!("signing key '{}'", kid)))
        } else {
            Ok(())
        }
    }
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

    fn fake_key(suffix: u8) -> Vec<u8> {
        vec![suffix; 32]
    }

    fn fake_pub(suffix: u8) -> Vec<u8> {
        vec![suffix; 64]
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn create_and_get_active(pool: sqlx::PgPool) {
        let repo = SigningKeyRepository::new(&pool);
        let k1 = repo
            .create("kid-1", &fake_key(1), &fake_pub(1))
            .await
            .expect("create ok");

        assert_eq!(k1.kid, "kid-1");
        assert_eq!(k1.algorithm, "RS256");
        assert!(k1.active);
        assert!(k1.retired_at.is_none());

        let active = repo.get_active().await.expect("query ok");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].kid, "kid-1");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn create_stores_key_bytes(pool: sqlx::PgPool) {
        let repo = SigningKeyRepository::new(&pool);
        let priv_bytes = fake_key(42);
        let pub_bytes = fake_pub(43);
        let k = repo
            .create("kid-bytes", &priv_bytes, &pub_bytes)
            .await
            .unwrap();
        assert_eq!(k.private_key, priv_bytes);
        assert_eq!(k.public_key, pub_bytes);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn create_duplicate_kid_returns_conflict(pool: sqlx::PgPool) {
        let repo = SigningKeyRepository::new(&pool);
        repo.create("kid-dup", &fake_key(1), &fake_pub(1))
            .await
            .unwrap();
        let err = repo
            .create("kid-dup", &fake_key(2), &fake_pub(2))
            .await
            .unwrap_err();
        assert!(matches!(err, RepoError::Conflict(_)));
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn get_newest_active_returns_latest(pool: sqlx::PgPool) {
        let repo = SigningKeyRepository::new(&pool);
        repo.create("kid-a", &fake_key(1), &fake_pub(1))
            .await
            .unwrap();
        repo.create("kid-b", &fake_key(2), &fake_pub(2))
            .await
            .unwrap();

        let newest = repo.get_newest_active().await.unwrap();
        assert!(newest.is_some());
        assert_eq!(newest.unwrap().kid, "kid-b");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn get_newest_active_returns_none_when_all_retired(pool: sqlx::PgPool) {
        let repo = SigningKeyRepository::new(&pool);
        repo.create("kid-x", &fake_key(9), &fake_pub(9))
            .await
            .unwrap();
        repo.retire("kid-x").await.unwrap();

        let newest = repo.get_newest_active().await.unwrap();
        assert!(newest.is_none());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn retire_removes_from_active_list(pool: sqlx::PgPool) {
        let repo = SigningKeyRepository::new(&pool);
        repo.create("kid-r", &fake_key(5), &fake_pub(5))
            .await
            .unwrap();

        repo.retire("kid-r").await.expect("retire ok");

        let active = repo.get_active().await.unwrap();
        assert!(active.is_empty());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn retire_stamps_retired_at(pool: sqlx::PgPool) {
        let repo = SigningKeyRepository::new(&pool);
        repo.create("kid-ts", &fake_key(7), &fake_pub(7))
            .await
            .unwrap();
        repo.retire("kid-ts").await.unwrap();

        // Fetch all (including retired) to verify the timestamp was set
        let rows = sqlx::query_as::<_, SigningKey>(
            "SELECT kid, algorithm, private_key, public_key, active, created_at, retired_at
             FROM signing_keys WHERE kid = 'kid-ts'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert!(!rows.active);
        assert!(rows.retired_at.is_some());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn retire_unknown_kid_returns_not_found(pool: sqlx::PgPool) {
        let repo = SigningKeyRepository::new(&pool);
        let err = repo.retire("nonexistent").await.unwrap_err();
        assert!(matches!(err, RepoError::NotFound(_)));
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn get_active_excludes_retired_keys(pool: sqlx::PgPool) {
        let repo = SigningKeyRepository::new(&pool);
        repo.create("kid-keep", &fake_key(1), &fake_pub(1))
            .await
            .unwrap();
        repo.create("kid-retire", &fake_key(2), &fake_pub(2))
            .await
            .unwrap();

        repo.retire("kid-retire").await.unwrap();

        let active = repo.get_active().await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].kid, "kid-keep");
    }
}
