use rsa::pkcs1::EncodeRsaPrivateKey;
use rsa::pkcs8::{DecodePublicKey, EncodePublicKey};
use rsa::RsaPrivateKey;
use uuid::Uuid;

use crate::error::CpError;

/// A freshly generated RSA-2048 keypair with an assigned `kid`.
pub struct GeneratedKeypair {
    pub kid: String,
    /// DER-encoded PKCS#8 private key bytes (plaintext — encrypt before storing).
    pub private_key_der: Vec<u8>,
    /// DER-encoded SubjectPublicKeyInfo bytes.
    pub public_key_der: Vec<u8>,
}

/// Generate a new RSA-2048 keypair.  The returned private key bytes are
/// **plaintext** — the caller must encrypt them before persisting.
pub fn generate_keypair() -> Result<GeneratedKeypair, CpError> {
    let mut rng = rand::thread_rng();
    let private_key =
        RsaPrivateKey::new(&mut rng, 2048).map_err(|e| CpError::KeyGeneration(e.to_string()))?;

    // Export as PKCS#1 DER — required by `jsonwebtoken::EncodingKey::from_rsa_der`.
    let private_key_der = private_key
        .to_pkcs1_der()
        .map_err(|e| CpError::KeyGeneration(e.to_string()))?
        .as_bytes()
        .to_vec();

    // Derive the public key from the private key and export as DER.
    let public_key = rsa::RsaPublicKey::from(&private_key);
    let public_key_der = public_key
        .to_public_key_der()
        .map_err(|e| CpError::KeyGeneration(e.to_string()))?
        .as_bytes()
        .to_vec();

    // Sanity check: verify we can round-trip back to an RsaPublicKey.
    rsa::RsaPublicKey::from_public_key_der(&public_key_der)
        .map_err(|e| CpError::KeyGeneration(format!("public key DER round-trip failed: {e}")))?;

    Ok(GeneratedKeypair {
        kid: Uuid::new_v4().to_string(),
        private_key_der,
        public_key_der,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_keypair_produces_valid_der() {
        let kp = generate_keypair().expect("keygen should succeed");
        assert!(!kp.kid.is_empty());
        assert!(!kp.private_key_der.is_empty());
        assert!(!kp.public_key_der.is_empty());

        // Kid must be a valid UUID.
        kp.kid.parse::<Uuid>().expect("kid must be a valid UUID");

        // Public key DER must parse back.
        rsa::RsaPublicKey::from_public_key_der(&kp.public_key_der)
            .expect("public key DER must be valid");
    }

    #[test]
    fn each_keypair_gets_a_unique_kid() {
        let a = generate_keypair().unwrap();
        let b = generate_keypair().unwrap();
        assert_ne!(a.kid, b.kid);
    }
}
