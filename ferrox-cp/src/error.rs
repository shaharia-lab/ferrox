/// Top-level errors for the `ferrox-cp` control plane.
#[derive(Debug, thiserror::Error)]
pub enum CpError {
    #[error("key generation failed: {0}")]
    KeyGeneration(String),

    #[error("private key decryption failed: {0}")]
    Decryption(String),

    #[error("JWKS serialisation failed: {0}")]
    Jwks(String),

    #[error("JWT signing failed: {0}")]
    JwtSigning(String),

    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("repository error: {0}")]
    Repository(#[from] crate::db::error::RepoError),

    #[error("configuration error: {0}")]
    Config(String),
}
