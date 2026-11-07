use aws_lc_rs::encoding::{AsDer, PublicKeyX509Der};
use aws_lc_rs::rsa::{KeyPair, KeySize};
use aws_lc_rs::signature::KeyPair as _;
use uuid::Uuid;

use crate::error::CpError;

/// A freshly generated RSA-2048 keypair with an assigned `kid`.
pub struct GeneratedKeypair {
    pub kid: String,
    /// DER-encoded PKCS#1 `RSAPrivateKey` bytes (plaintext — encrypt before
    /// storing). PKCS#1 is what `jsonwebtoken::EncodingKey::from_rsa_der`
    /// expects, and what every existing stored key uses.
    pub private_key_der: Vec<u8>,
    /// DER-encoded SubjectPublicKeyInfo bytes.
    pub public_key_der: Vec<u8>,
}

/// Generate a new RSA-2048 keypair.  The returned private key bytes are
/// **plaintext** — the caller must encrypt them before persisting.
pub fn generate_keypair() -> Result<GeneratedKeypair, CpError> {
    let key_pair =
        KeyPair::generate(KeySize::Rsa2048).map_err(|e| CpError::KeyGeneration(e.to_string()))?;

    // aws-lc-rs exports private keys only as PKCS#8; unwrap it to the PKCS#1
    // `RSAPrivateKey` the signer and the stored-key format require.
    let pkcs8 = key_pair
        .as_der()
        .map_err(|e| CpError::KeyGeneration(e.to_string()))?;
    let private_key_der = pkcs1_from_pkcs8(pkcs8.as_ref())?.to_vec();

    // Sanity check: the PKCS#1 bytes load exactly as the signer loads them.
    KeyPair::from_der(&private_key_der)
        .map_err(|e| CpError::KeyGeneration(format!("PKCS#1 round-trip failed: {e}")))?;

    let public_key_der = AsDer::<PublicKeyX509Der>::as_der(key_pair.public_key())
        .map_err(|e| CpError::KeyGeneration(e.to_string()))?
        .as_ref()
        .to_vec();

    Ok(GeneratedKeypair {
        kid: Uuid::new_v4().to_string(),
        private_key_der,
        public_key_der,
    })
}

/// `rsaEncryption` AlgorithmIdentifier (OID 1.2.840.113549.1.1.1 + NULL), DER.
const RSA_ALGORITHM_ID: &[u8] = &[
    0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00,
];

/// Extract the PKCS#1 `RSAPrivateKey` from a PKCS#8 v1 `PrivateKeyInfo`:
/// `SEQUENCE { INTEGER 0, AlgorithmIdentifier(rsaEncryption), OCTET STRING }`.
/// Strict: any other shape (version, algorithm, trailing bytes) is rejected.
fn pkcs1_from_pkcs8(der: &[u8]) -> Result<&[u8], CpError> {
    let bad = |what: &str| CpError::KeyGeneration(format!("unexpected PKCS#8 encoding: {what}"));

    let (body, rest) = der_tlv(der, 0x30).ok_or_else(|| bad("outer SEQUENCE"))?;
    if !rest.is_empty() {
        return Err(bad("trailing data"));
    }
    let (version, body) = der_tlv(body, 0x02).ok_or_else(|| bad("version"))?;
    if version != [0x00] {
        return Err(bad("version is not v1"));
    }
    let body = body
        .strip_prefix(RSA_ALGORITHM_ID)
        .ok_or_else(|| bad("algorithm is not rsaEncryption"))?;
    let (key, rest) = der_tlv(body, 0x04).ok_or_else(|| bad("privateKey OCTET STRING"))?;
    if !rest.is_empty() {
        return Err(bad("unexpected attributes"));
    }
    Ok(key)
}

/// Split one DER TLV with tag `tag` off `input`, returning `(value, rest)`.
/// Supports definite short- and long-form lengths (up to 4 length bytes).
fn der_tlv(input: &[u8], tag: u8) -> Option<(&[u8], &[u8])> {
    let (&t, input) = input.split_first()?;
    if t != tag {
        return None;
    }
    let (&first, input) = input.split_first()?;
    let (len, input) = if first < 0x80 {
        (usize::from(first), input)
    } else {
        let n = usize::from(first & 0x7f);
        if n == 0 || n > 4 || input.len() < n {
            return None;
        }
        let len = input[..n]
            .iter()
            .fold(0usize, |acc, &b| (acc << 8) | usize::from(b));
        (len, &input[n..])
    };
    (input.len() >= len).then(|| input.split_at(len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::rsa::PublicKey;

    #[test]
    fn generate_keypair_produces_valid_der() {
        let kp = generate_keypair().expect("keygen should succeed");
        assert!(!kp.kid.is_empty());
        kp.kid.parse::<Uuid>().expect("kid must be a valid UUID");

        // Private key is PKCS#1 RSAPrivateKey (what the signer loads).
        let key_pair = KeyPair::from_der(&kp.private_key_der).expect("PKCS#1 private key");
        assert_eq!(key_pair.public_modulus_len(), 256, "RSA-2048");

        // Public key is SPKI and matches the private key.
        let public = PublicKey::from_der(&kp.public_key_der).expect("SPKI public key");
        assert_eq!(public.as_ref(), key_pair.public_key().as_ref());
    }

    #[test]
    fn each_keypair_gets_a_unique_kid() {
        let a = generate_keypair().unwrap();
        let b = generate_keypair().unwrap();
        assert_ne!(a.kid, b.kid);
    }

    #[test]
    fn pkcs1_from_pkcs8_rejects_malformed_input() {
        let kp = KeyPair::generate(KeySize::Rsa2048).unwrap();
        let pkcs8 = kp.as_der().unwrap().as_ref().to_vec();
        assert!(pkcs1_from_pkcs8(&pkcs8).is_ok());

        let mut trailing = pkcs8.clone();
        trailing.push(0);
        assert!(pkcs1_from_pkcs8(&trailing).is_err(), "trailing bytes");
        assert!(
            pkcs1_from_pkcs8(&pkcs8[..pkcs8.len() - 1]).is_err(),
            "truncated"
        );
        assert!(pkcs1_from_pkcs8(&[]).is_err(), "empty");
        assert!(pkcs1_from_pkcs8(b"not der").is_err(), "garbage");
    }
}
