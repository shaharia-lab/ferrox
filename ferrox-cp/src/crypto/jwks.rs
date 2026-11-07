use aws_lc_rs::rsa::PublicKey;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::Serialize;
use utoipa::ToSchema;

use crate::error::CpError;

/// A single JWK entry as defined by RFC 7517 for an RSA signing key.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Jwk {
    pub kty: String,
    #[serde(rename = "use")]
    pub key_use: String,
    pub alg: String,
    pub kid: String,
    /// Base64url-encoded modulus.
    pub n: String,
    /// Base64url-encoded public exponent.
    pub e: String,
}

/// Convert DER-encoded SubjectPublicKeyInfo bytes to a [`Jwk`].
pub fn public_key_to_jwk(kid: &str, public_key_der: &[u8]) -> Result<Jwk, CpError> {
    let public_key = PublicKey::from_der(public_key_der)
        .map_err(|e| CpError::Jwks(format!("failed to parse public key DER: {e}")))?;

    // Big-endian, no leading zeros — exactly the RFC 7518 §6.3.1 encoding.
    let n = URL_SAFE_NO_PAD.encode(public_key.modulus().big_endian_without_leading_zero());
    let e = URL_SAFE_NO_PAD.encode(public_key.exponent().big_endian_without_leading_zero());

    Ok(Jwk {
        kty: "RSA".to_string(),
        key_use: "sig".to_string(),
        alg: "RS256".to_string(),
        kid: kid.to_string(),
        n,
        e,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::keys::generate_keypair;

    #[test]
    fn jwk_fields_are_populated_correctly() {
        let kp = generate_keypair().expect("keygen ok");
        let jwk = public_key_to_jwk(&kp.kid, &kp.public_key_der).expect("jwk ok");
        assert_eq!(jwk.kty, "RSA");
        assert_eq!(jwk.key_use, "sig");
        assert_eq!(jwk.alg, "RS256");
        assert_eq!(jwk.kid, kp.kid);
        // Modulus and exponent must be non-empty base64url strings (no padding).
        assert!(!jwk.n.is_empty());
        assert!(!jwk.e.is_empty());
        assert!(!jwk.n.contains('='));
        assert!(!jwk.e.contains('='));
    }

    #[test]
    fn invalid_der_returns_error() {
        let result = public_key_to_jwk("kid", b"not valid DER");
        assert!(result.is_err());
    }

    #[test]
    fn jwk_serialises_to_expected_json_shape() {
        let kp = generate_keypair().expect("keygen ok");
        let jwk = public_key_to_jwk(&kp.kid, &kp.public_key_der).expect("jwk ok");
        let json: serde_json::Value = serde_json::to_value(&jwk).unwrap();
        assert_eq!(json["kty"], "RSA");
        assert_eq!(json["use"], "sig");
        assert_eq!(json["alg"], "RS256");
        assert!(json["n"].is_string());
        assert!(json["e"].is_string());
    }

    /// A key generated and stored by the previous `rsa`-crate implementation
    /// must publish the exact same JWK, so already-issued tokens keep verifying.
    #[test]
    fn legacy_stored_key_publishes_identical_jwk() {
        use base64::engine::general_purpose::STANDARD;
        let spki = STANDARD
            .decode(include_str!("testdata/legacy_public_spki.der.b64").trim())
            .unwrap();
        let want: serde_json::Value =
            serde_json::from_str(include_str!("testdata/legacy_jwk.json")).unwrap();
        let jwk = public_key_to_jwk("legacy", &spki).expect("jwk ok");
        assert_eq!(jwk.n, want["n"]);
        assert_eq!(jwk.e, want["e"]);
    }
}
