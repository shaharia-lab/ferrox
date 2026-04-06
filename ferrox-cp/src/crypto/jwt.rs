use chrono::Utc;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::models::{Client, SigningKey};
use crate::error::CpError;

/// A signed JWT and its metadata.
pub struct SignedToken {
    /// The compact serialised JWT string (`header.payload.signature`).
    pub token: String,
    /// The `jti` claim embedded in the token.
    pub jti: String,
    /// UTC expiry timestamp (Unix seconds).
    pub expires_at: i64,
}

/// Rate-limit sub-object inside the `ferrox` custom claim.
#[derive(Debug, Serialize, Deserialize)]
struct RateLimitClaim {
    requests_per_minute: i32,
    burst: i32,
}

/// The `ferrox` custom claim namespace.
#[derive(Debug, Serialize, Deserialize)]
struct FerroxClaim {
    tenant_id: String,
    /// UUID of the client in the control-plane `clients` table.
    client_id: String,
    allowed_models: Vec<String>,
    rate_limit: RateLimitClaim,
}

/// Full JWT claims payload.
#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    sub: String,
    iss: String,
    aud: String,
    exp: i64,
    iat: i64,
    jti: String,
    ferrox: FerroxClaim,
}

/// Signs JWTs for control-plane clients.
///
/// The signing key is loaded from the database at startup and cached here.
/// Construct one instance per process (or refresh after key rotation).
pub struct JwtSigner {
    encoding_key: EncodingKey,
    kid: String,
    issuer: String,
}

impl JwtSigner {
    /// Create a `JwtSigner` from a [`SigningKey`] row whose private key bytes
    /// have already been decrypted.
    pub fn new(
        signing_key: &SigningKey,
        decrypted_private_key_der: &[u8],
        issuer: String,
    ) -> Result<Self, CpError> {
        let encoding_key = EncodingKey::from_rsa_der(decrypted_private_key_der);

        Ok(Self {
            encoding_key,
            kid: signing_key.kid.clone(),
            issuer,
        })
    }

    /// Sign a JWT for `client`.  The token is valid for `client.token_ttl_seconds`.
    pub fn sign(&self, client: &Client) -> Result<SignedToken, CpError> {
        let now = Utc::now().timestamp();
        let expires_at = now + i64::from(client.token_ttl_seconds);
        let jti = Uuid::new_v4().to_string();

        let claims = Claims {
            sub: client.name.clone(),
            iss: self.issuer.clone(),
            aud: "ferrox".to_string(),
            exp: expires_at,
            iat: now,
            jti: jti.clone(),
            ferrox: FerroxClaim {
                tenant_id: client.name.clone(),
                client_id: client.id.to_string(),
                allowed_models: client.allowed_models.clone(),
                rate_limit: RateLimitClaim {
                    requests_per_minute: client.rpm,
                    burst: client.burst,
                },
            },
        };

        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(self.kid.clone());

        let token = encode(&header, &claims, &self.encoding_key)
            .map_err(|e| CpError::JwtSigning(e.to_string()))?;

        Ok(SignedToken {
            token,
            jti,
            expires_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::encrypt::encrypt_private_key;
    use crate::crypto::keys::generate_keypair;
    use jsonwebtoken::{decode, DecodingKey, Validation};
    use rsa::pkcs8::DecodePublicKey;
    use rsa::RsaPublicKey;

    fn make_client() -> Client {
        Client {
            id: Uuid::new_v4(),
            name: "test-client".to_string(),
            description: None,
            key_prefix: "pfxtest1".to_string(),
            api_key_hash: "hash".to_string(),
            allowed_models: vec!["gpt-4".to_string(), "claude-3".to_string()],
            rpm: 300,
            burst: 30,
            token_ttl_seconds: 900,
            active: true,
            created_at: Utc::now(),
            revoked_at: None,
        }
    }

    fn make_signing_key_and_signer() -> (SigningKey, JwtSigner) {
        let kp = generate_keypair().expect("keygen ok");
        let enc_key = [42u8; 32];
        let encrypted_private_key = encrypt_private_key(&kp.private_key_der, &enc_key);

        let signing_key = SigningKey {
            kid: kp.kid.clone(),
            algorithm: "RS256".to_string(),
            private_key: encrypted_private_key,
            public_key: kp.public_key_der.clone(),
            active: true,
            created_at: Utc::now(),
            retired_at: None,
        };

        let signer = JwtSigner::new(
            &signing_key,
            &kp.private_key_der,
            "https://ferrox-cp".to_string(),
        )
        .expect("signer creation ok");

        (signing_key, signer)
    }

    #[test]
    fn sign_produces_verifiable_jwt() {
        let (signing_key, signer) = make_signing_key_and_signer();
        let client = make_client();
        let signed = signer.sign(&client).expect("sign ok");

        assert!(!signed.token.is_empty());
        assert!(!signed.jti.is_empty());
        assert!(signed.expires_at > Utc::now().timestamp());

        // Verify using the public key — mirrors what the gateway does.
        // jsonwebtoken::DecodingKey::from_rsa_der expects PKCS#1 DER.
        let public_key =
            RsaPublicKey::from_public_key_der(&signing_key.public_key).expect("public key ok");
        let public_key_pkcs1_der = {
            use rsa::pkcs1::EncodeRsaPublicKey;
            public_key.to_pkcs1_der().unwrap().as_bytes().to_vec()
        };

        let decoding_key = DecodingKey::from_rsa_der(&public_key_pkcs1_der);
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&["ferrox"]);

        let token_data =
            decode::<Claims>(&signed.token, &decoding_key, &validation).expect("jwt must verify");

        assert_eq!(token_data.claims.sub, client.name);
        assert_eq!(token_data.claims.iss, "https://ferrox-cp");
        assert_eq!(token_data.claims.aud, "ferrox");
        assert_eq!(
            token_data.claims.ferrox.allowed_models,
            client.allowed_models
        );
        assert_eq!(
            token_data.claims.ferrox.rate_limit.requests_per_minute,
            client.rpm
        );
        assert_eq!(token_data.claims.ferrox.rate_limit.burst, client.burst);
        assert_eq!(token_data.claims.ferrox.tenant_id, client.name);
        assert_eq!(token_data.claims.ferrox.client_id, client.id.to_string());
    }

    #[test]
    fn each_token_has_a_unique_jti() {
        let (_, signer) = make_signing_key_and_signer();
        let client = make_client();
        let t1 = signer.sign(&client).expect("sign ok");
        let t2 = signer.sign(&client).expect("sign ok");
        assert_ne!(t1.jti, t2.jti, "each token must have a unique jti");
    }

    #[test]
    fn token_expiry_matches_client_ttl() {
        let (_, signer) = make_signing_key_and_signer();
        let client = make_client(); // token_ttl_seconds = 900
        let before = Utc::now().timestamp();
        let signed = signer.sign(&client).expect("sign ok");
        let after = Utc::now().timestamp();

        assert!(signed.expires_at >= before + 900);
        assert!(signed.expires_at <= after + 900);
    }

    #[test]
    fn kid_header_matches_signing_key() {
        let (signing_key, signer) = make_signing_key_and_signer();
        let client = make_client();
        let signed = signer.sign(&client).expect("sign ok");

        // Decode header without verification to check kid.
        let header = jsonwebtoken::decode_header(&signed.token).expect("header ok");
        assert_eq!(header.kid.as_deref(), Some(signing_key.kid.as_str()));
    }

    #[test]
    fn token_is_not_valid_with_different_key() {
        let (_, signer) = make_signing_key_and_signer();
        let client = make_client();
        let signed = signer.sign(&client).expect("sign ok");

        // Generate a completely different keypair and try to verify.
        // Convert SPKI DER → PKCS#1 DER for DecodingKey.
        let other_kp = generate_keypair().expect("keygen ok");
        let other_public_pkcs1 = {
            use rsa::pkcs1::EncodeRsaPublicKey;
            let pk = RsaPublicKey::from_public_key_der(&other_kp.public_key_der).unwrap();
            pk.to_pkcs1_der().unwrap().as_bytes().to_vec()
        };
        let decoding_key = DecodingKey::from_rsa_der(&other_public_pkcs1);
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&["ferrox"]);
        let result = decode::<Claims>(&signed.token, &decoding_key, &validation);
        assert!(
            result.is_err(),
            "token must not verify with a different key"
        );
    }
}
