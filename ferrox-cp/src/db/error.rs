/// Errors returned by the repository layer.
#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    /// A record with a conflicting unique key already exists.
    #[error("conflict: {0}")]
    Conflict(String),

    /// The requested record does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// An unexpected database error occurred.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}
