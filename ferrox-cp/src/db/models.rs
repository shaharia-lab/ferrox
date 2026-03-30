use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use uuid::Uuid;

/// A tenant / API client registered in the control plane.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Client {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    /// First 8 characters of the raw API key, stored in plain text.
    /// Used as a fast lookup discriminator before the full bcrypt comparison.
    pub key_prefix: String,
    /// bcrypt hash of the full raw API key.
    pub api_key_hash: String,
    /// Model aliases this client is permitted to call.  `["*"]` means all.
    pub allowed_models: Vec<String>,
    pub rpm: i32,
    pub burst: i32,
    pub token_ttl_seconds: i32,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// An RSA signing keypair used to issue JWTs.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SigningKey {
    /// UUID used as the JWT `kid` header.
    pub kid: String,
    /// Always `"RS256"` in the current implementation.
    pub algorithm: String,
    /// AES-256-GCM encrypted private key bytes.
    pub private_key: Vec<u8>,
    /// DER-encoded public key bytes.
    pub public_key: Vec<u8>,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub retired_at: Option<DateTime<Utc>>,
}

/// A single entry in the immutable audit log.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AuditEntry {
    pub id: i64,
    pub client_id: Option<Uuid>,
    pub event: AuditEvent,
    pub metadata: Option<JsonValue>,
    pub created_at: DateTime<Utc>,
}

/// Well-known audit event types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditEvent {
    TokenIssued,
    ClientCreated,
    ClientRevoked,
    KeyRotated,
    #[serde(untagged)]
    Other(String),
}

impl AuditEvent {
    pub fn as_str(&self) -> &str {
        match self {
            Self::TokenIssued => "token_issued",
            Self::ClientCreated => "client_created",
            Self::ClientRevoked => "client_revoked",
            Self::KeyRotated => "key_rotated",
            Self::Other(s) => s.as_str(),
        }
    }
}

impl<'r> sqlx::Decode<'r, sqlx::Postgres> for AuditEvent {
    fn decode(
        value: sqlx::postgres::PgValueRef<'r>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let s: &str = sqlx::Decode::<sqlx::Postgres>::decode(value)?;
        Ok(match s {
            "token_issued" => Self::TokenIssued,
            "client_created" => Self::ClientCreated,
            "client_revoked" => Self::ClientRevoked,
            "key_rotated" => Self::KeyRotated,
            other => Self::Other(other.to_string()),
        })
    }
}

impl sqlx::Type<sqlx::Postgres> for AuditEvent {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        <String as sqlx::Type<sqlx::Postgres>>::type_info()
    }
}

impl sqlx::Encode<'_, sqlx::Postgres> for AuditEvent {
    fn encode_by_ref(
        &self,
        buf: &mut sqlx::postgres::PgArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, Box<dyn std::error::Error + Send + Sync>> {
        sqlx::Encode::<sqlx::Postgres>::encode_by_ref(&self.as_str().to_string(), buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_event_as_str_roundtrip() {
        assert_eq!(AuditEvent::TokenIssued.as_str(), "token_issued");
        assert_eq!(AuditEvent::ClientCreated.as_str(), "client_created");
        assert_eq!(AuditEvent::ClientRevoked.as_str(), "client_revoked");
        assert_eq!(AuditEvent::KeyRotated.as_str(), "key_rotated");
        assert_eq!(AuditEvent::Other("custom".into()).as_str(), "custom");
    }

    #[test]
    fn audit_event_serde_roundtrip() {
        for event in [
            AuditEvent::TokenIssued,
            AuditEvent::ClientCreated,
            AuditEvent::ClientRevoked,
            AuditEvent::KeyRotated,
            AuditEvent::Other("foo_bar".into()),
        ] {
            let json = serde_json::to_string(&event).unwrap();
            let back: AuditEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(event, back);
        }
    }
}
