/// Runtime configuration for the control plane.
///
/// All fields can be supplied via environment variables (names match the field
/// names, uppercased).  `database_url` is the only strictly required field at
/// startup; the others have sensible defaults.
#[derive(Clone)]
pub struct CpConfig {
    /// PostgreSQL connection string, e.g. `postgres://user:pass@host/db`.
    pub database_url: String,

    /// Issuer claim (`iss`) embedded in every JWT we sign.
    /// Defaults to `"https://ferrox-cp"`.
    pub cp_issuer: String,

    /// 32-byte hex-encoded key used to AES-256-GCM encrypt private keys at rest.
    /// Must be exactly 64 hex characters.
    pub cp_encryption_key: String,

    /// Static bearer token that protects all admin REST endpoints.
    /// Set via the `CP_ADMIN_KEY` environment variable.
    pub admin_key: String,

    /// TCP port the control-plane HTTP server listens on.  Defaults to `9090`.
    pub port: u16,
}

/// Manual `Debug` impl that redacts secrets so they never appear in logs or
/// panic output.  `database_url` is redacted entirely because it embeds the
/// DB password; `cp_encryption_key` and `admin_key` are replaced with a fixed
/// placeholder.
impl std::fmt::Debug for CpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CpConfig")
            .field("database_url", &"[redacted]")
            .field("cp_issuer", &self.cp_issuer)
            .field("cp_encryption_key", &"[redacted]")
            .field("admin_key", &"[redacted]")
            .field("port", &self.port)
            .finish()
    }
}

impl CpConfig {
    /// Build a `CpConfig` from environment variables.
    ///
    /// Returns an error if any required variable is missing.
    pub fn from_env() -> Result<Self, ConfigError> {
        let database_url =
            std::env::var("DATABASE_URL").map_err(|_| ConfigError::Missing("DATABASE_URL"))?;
        let cp_encryption_key = std::env::var("CP_ENCRYPTION_KEY")
            .map_err(|_| ConfigError::Missing("CP_ENCRYPTION_KEY"))?;
        let admin_key =
            std::env::var("CP_ADMIN_KEY").map_err(|_| ConfigError::Missing("CP_ADMIN_KEY"))?;

        if cp_encryption_key.len() != 64 {
            return Err(ConfigError::Invalid(
                "CP_ENCRYPTION_KEY must be exactly 64 hex characters (32 bytes)",
            ));
        }

        Ok(Self {
            database_url,
            cp_issuer: std::env::var("CP_ISSUER")
                .unwrap_or_else(|_| "https://ferrox-cp".to_string()),
            cp_encryption_key,
            admin_key,
            port: std::env::var("CP_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(9090),
        })
    }
}

/// Errors that can occur while loading [`CpConfig`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("required environment variable {0} is not set")]
    Missing(&'static str),

    #[error("invalid configuration: {0}")]
    Invalid(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialise all config tests that mutate global env vars so they do not
    // race with each other or with DB integration tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Save the current DATABASE_URL, set fake config vars, return the saved URL.
    fn set_required(database_url: &str, key: &str, admin: &str) -> Option<String> {
        let saved = std::env::var("DATABASE_URL").ok();
        std::env::set_var("DATABASE_URL", database_url);
        std::env::set_var("CP_ENCRYPTION_KEY", key);
        std::env::set_var("CP_ADMIN_KEY", admin);
        saved
    }

    /// Remove config vars and restore DATABASE_URL to its original value so
    /// DB integration tests that run after can still connect.
    fn unset_required(saved_database_url: Option<String>) {
        match saved_database_url {
            Some(v) => std::env::set_var("DATABASE_URL", v),
            None => std::env::remove_var("DATABASE_URL"),
        }
        std::env::remove_var("CP_ENCRYPTION_KEY");
        std::env::remove_var("CP_ADMIN_KEY");
        std::env::remove_var("CP_ISSUER");
        std::env::remove_var("CP_PORT");
    }

    #[test]
    fn from_env_succeeds_with_all_required_vars() {
        let _lock = ENV_LOCK.lock().unwrap();
        let key = "a".repeat(64);
        let saved = set_required("postgres://localhost/test", &key, "admin-secret");
        let cfg = CpConfig::from_env().expect("should succeed");
        assert_eq!(cfg.database_url, "postgres://localhost/test");
        assert_eq!(cfg.cp_issuer, "https://ferrox-cp");
        assert_eq!(cfg.port, 9090);
        unset_required(saved);
    }

    #[test]
    fn from_env_uses_custom_issuer_and_port() {
        let _lock = ENV_LOCK.lock().unwrap();
        let key = "b".repeat(64);
        let saved = set_required("postgres://localhost/test", &key, "k");
        std::env::set_var("CP_ISSUER", "https://my-issuer");
        std::env::set_var("CP_PORT", "8443");
        let cfg = CpConfig::from_env().expect("should succeed");
        assert_eq!(cfg.cp_issuer, "https://my-issuer");
        assert_eq!(cfg.port, 8443);
        unset_required(saved);
    }

    #[test]
    fn from_env_fails_when_database_url_missing() {
        let _lock = ENV_LOCK.lock().unwrap();
        let saved = std::env::var("DATABASE_URL").ok();
        std::env::remove_var("DATABASE_URL");
        let key = "c".repeat(64);
        std::env::set_var("CP_ENCRYPTION_KEY", &key);
        std::env::set_var("CP_ADMIN_KEY", "k");
        let err = CpConfig::from_env().unwrap_err();
        assert!(err.to_string().contains("DATABASE_URL"));
        unset_required(saved);
    }

    #[test]
    fn debug_output_redacts_secrets() {
        let _lock = ENV_LOCK.lock().unwrap();
        let key = "d".repeat(64);
        let saved = set_required("postgres://user:secret@host/db", &key, "super-secret-admin");
        let cfg = CpConfig::from_env().expect("should succeed");
        let debug = format!("{:?}", cfg);
        assert!(
            !debug.contains("secret"),
            "debug output must not contain the raw password or admin key: {debug}"
        );
        assert!(
            !debug.contains(&key),
            "debug output must not contain the raw encryption key: {debug}"
        );
        assert!(
            debug.contains("[redacted]"),
            "debug output should show [redacted]: {debug}"
        );
        unset_required(saved);
    }

    #[test]
    fn from_env_fails_when_encryption_key_wrong_length() {
        let _lock = ENV_LOCK.lock().unwrap();
        let saved = set_required("postgres://localhost/test", "tooshort", "k");
        let err = CpConfig::from_env().unwrap_err();
        assert!(err.to_string().contains("64 hex characters"));
        unset_required(saved);
    }
}
