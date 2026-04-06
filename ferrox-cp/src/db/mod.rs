pub mod audit_repo;
pub mod client_repo;
pub mod error;
pub mod models;
pub mod signing_key_repo;
pub mod usage_repo;

// Re-export commonly used types for convenience.
// These will be consumed by the HTTP handler layer in later milestones.
#[allow(unused_imports)]
pub use audit_repo::AuditRepository;
#[allow(unused_imports)]
pub use client_repo::ClientRepository;
#[allow(unused_imports)]
pub use error::RepoError;
#[allow(unused_imports)]
pub use models::{AuditEntry, AuditEvent, Client, SigningKey, UsageRecord, UsageSummary};
#[allow(unused_imports)]
pub use signing_key_repo::SigningKeyRepository;
#[allow(unused_imports)]
pub use usage_repo::UsageRepository;
