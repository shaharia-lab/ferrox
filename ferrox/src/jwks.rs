use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::{AlgorithmParameters, JwkSet, KeyAlgorithm};
use jsonwebtoken::{Algorithm, DecodingKey};
use tokio::sync::RwLock;

use crate::config::TrustedIssuerConfig;

struct IssuerCache {
    #[allow(dead_code)]
    jwks_uri: String,
    keys: JwkSet,
    fetched_at: Instant,
}

pub struct JwksCache {
    inner: Arc<RwLock<HashMap<String, IssuerCache>>>,
    issuers: Vec<TrustedIssuerConfig>,
    ttl: Duration,
    client: reqwest::Client,
}

impl JwksCache {
    pub fn new(issuers: Vec<TrustedIssuerConfig>, ttl_secs: u64, client: reqwest::Client) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            issuers,
            ttl: Duration::from_secs(ttl_secs),
            client,
        }
    }

    pub async fn prefetch_all(&self) {
        for issuer_cfg in &self.issuers {
            match self.fetch_issuer(issuer_cfg).await {
                Ok(keys) => {
                    let mut map = self.inner.write().await;
                    map.insert(
                        issuer_cfg.issuer.clone(),
                        IssuerCache {
                            jwks_uri: issuer_cfg.jwks_uri.clone(),
                            keys,
                            fetched_at: Instant::now(),
                        },
                    );
                    tracing::info!(issuer = %issuer_cfg.issuer, "JWKS prefetched");
                }
                Err(e) => {
                    tracing::warn!(
                        issuer = %issuer_cfg.issuer,
                        error = %e,
                        "Failed to prefetch JWKS — will retry on first request"
                    );
                }
            }
        }
    }

    async fn fetch_issuer(&self, cfg: &TrustedIssuerConfig) -> Result<JwkSet, anyhow::Error> {
        let resp = self
            .client
            .get(&cfg.jwks_uri)
            .send()
            .await?
            .error_for_status()?
            .json::<JwkSet>()
            .await?;
        Ok(resp)
    }

    /// Returns a DecodingKey and matching Algorithm for the given issuer + kid.
    /// On cache miss or TTL expiry, refreshes from the JWKS URI.
    /// On refresh failure, falls back to stale cache.
    pub async fn get_decoding_key(
        &self,
        issuer: &str,
        kid: Option<&str>,
    ) -> Option<(DecodingKey, Algorithm)> {
        let issuer_cfg = self.issuers.iter().find(|i| i.issuer == issuer)?;

        // Try cache first (if not stale)
        {
            let map = self.inner.read().await;
            if let Some(cached) = map.get(issuer) {
                if cached.fetched_at.elapsed() < self.ttl {
                    if let Some(found) = find_key_in_set(&cached.keys, kid) {
                        return Some(found);
                    }
                }
            }
        }

        // Cache miss, stale, or kid not found — refresh
        tracing::debug!(issuer = %issuer, kid = ?kid, "Refreshing JWKS");
        match self.fetch_issuer(issuer_cfg).await {
            Ok(keys) => {
                let found = find_key_in_set(&keys, kid);
                let mut map = self.inner.write().await;
                map.insert(
                    issuer.to_string(),
                    IssuerCache {
                        jwks_uri: issuer_cfg.jwks_uri.clone(),
                        keys,
                        fetched_at: Instant::now(),
                    },
                );
                found
            }
            Err(e) => {
                tracing::warn!(issuer = %issuer, error = %e, "JWKS refresh failed — serving stale if available");
                let map = self.inner.read().await;
                map.get(issuer).and_then(|c| find_key_in_set(&c.keys, kid))
            }
        }
    }

    /// Spawns a background task that refreshes all JWKS caches at 80% of the configured TTL.
    pub fn spawn_refresh_task(self: Arc<Self>) {
        let refresh_interval = self.ttl.mul_f32(0.8);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(refresh_interval);
            ticker.tick().await; // skip immediate first tick
            loop {
                ticker.tick().await;
                for issuer_cfg in &self.issuers {
                    match self.fetch_issuer(issuer_cfg).await {
                        Ok(keys) => {
                            let mut map = self.inner.write().await;
                            let entry = map.entry(issuer_cfg.issuer.clone()).or_insert_with(|| {
                                IssuerCache {
                                    jwks_uri: issuer_cfg.jwks_uri.clone(),
                                    keys: JwkSet { keys: vec![] },
                                    fetched_at: Instant::now(),
                                }
                            });
                            entry.keys = keys;
                            entry.fetched_at = Instant::now();
                            tracing::debug!(issuer = %issuer_cfg.issuer, "JWKS background refresh OK");
                        }
                        Err(e) => {
                            tracing::warn!(
                                issuer = %issuer_cfg.issuer,
                                error = %e,
                                "Background JWKS refresh failed"
                            );
                        }
                    }
                }
            }
        });
    }
}

fn find_key_in_set(jwks: &JwkSet, kid: Option<&str>) -> Option<(DecodingKey, Algorithm)> {
    let jwk = match kid {
        Some(k) => jwks.find(k).or_else(|| jwks.keys.first()),
        None => jwks.keys.first(),
    }?;

    let alg = jwk
        .common
        .key_algorithm
        .as_ref()
        .and_then(key_alg_to_algorithm)
        .unwrap_or_else(|| infer_alg_from_params(&jwk.algorithm));

    let decoding_key = DecodingKey::from_jwk(jwk).ok()?;
    Some((decoding_key, alg))
}

fn key_alg_to_algorithm(ka: &KeyAlgorithm) -> Option<Algorithm> {
    match ka {
        KeyAlgorithm::RS256 => Some(Algorithm::RS256),
        KeyAlgorithm::RS384 => Some(Algorithm::RS384),
        KeyAlgorithm::RS512 => Some(Algorithm::RS512),
        KeyAlgorithm::ES256 => Some(Algorithm::ES256),
        KeyAlgorithm::ES384 => Some(Algorithm::ES384),
        KeyAlgorithm::PS256 => Some(Algorithm::PS256),
        KeyAlgorithm::PS384 => Some(Algorithm::PS384),
        KeyAlgorithm::PS512 => Some(Algorithm::PS512),
        KeyAlgorithm::EdDSA => Some(Algorithm::EdDSA),
        _ => None,
    }
}

fn infer_alg_from_params(params: &AlgorithmParameters) -> Algorithm {
    match params {
        AlgorithmParameters::RSA(_) => Algorithm::RS256,
        AlgorithmParameters::EllipticCurve(_) => Algorithm::ES256,
        AlgorithmParameters::OctetKeyPair(_) => Algorithm::EdDSA,
        AlgorithmParameters::OctetKey(_) => Algorithm::HS256,
    }
}

// ── Test helpers ──────────────────────────────────────────────────────────────

#[cfg(test)]
impl JwksCache {
    /// Directly seed the cache with a pre-built JwkSet. Test use only.
    pub async fn seed_for_test(&self, issuer: &str, jwks_uri: &str, keys: JwkSet) {
        let mut map = self.inner.write().await;
        map.insert(
            issuer.to_string(),
            IssuerCache {
                jwks_uri: jwks_uri.to_string(),
                keys,
                fetched_at: Instant::now(),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

    // ── Helpers ───────────────────────────────────────────────────────────────

    const TEST_SECRET: &[u8] = b"test-secret-for-jwks-unit-tests";
    const TEST_KID: &str = "test-kid-1";
    const TEST_ISSUER: &str = "https://test.example.com";

    fn oct_jwks(kid: &str, secret: &[u8]) -> JwkSet {
        let k = URL_SAFE_NO_PAD.encode(secret);
        serde_json::from_str(&format!(
            r#"{{"keys":[{{"kty":"oct","kid":"{}","alg":"HS256","k":"{}"}}]}}"#,
            kid, k
        ))
        .unwrap()
    }

    fn test_issuer_cfg() -> TrustedIssuerConfig {
        TrustedIssuerConfig {
            issuer: TEST_ISSUER.to_string(),
            jwks_uri: format!("{}/jwks.json", TEST_ISSUER),
            audience: None,
        }
    }

    // ── key_alg_to_algorithm ──────────────────────────────────────────────────

    #[test]
    fn key_alg_rs256_maps_correctly() {
        assert_eq!(
            key_alg_to_algorithm(&KeyAlgorithm::RS256),
            Some(Algorithm::RS256)
        );
    }

    #[test]
    fn key_alg_rs384_maps_correctly() {
        assert_eq!(
            key_alg_to_algorithm(&KeyAlgorithm::RS384),
            Some(Algorithm::RS384)
        );
    }

    #[test]
    fn key_alg_rs512_maps_correctly() {
        assert_eq!(
            key_alg_to_algorithm(&KeyAlgorithm::RS512),
            Some(Algorithm::RS512)
        );
    }

    #[test]
    fn key_alg_es256_maps_correctly() {
        assert_eq!(
            key_alg_to_algorithm(&KeyAlgorithm::ES256),
            Some(Algorithm::ES256)
        );
    }

    #[test]
    fn key_alg_es384_maps_correctly() {
        assert_eq!(
            key_alg_to_algorithm(&KeyAlgorithm::ES384),
            Some(Algorithm::ES384)
        );
    }

    #[test]
    fn key_alg_ps256_maps_correctly() {
        assert_eq!(
            key_alg_to_algorithm(&KeyAlgorithm::PS256),
            Some(Algorithm::PS256)
        );
    }

    #[test]
    fn key_alg_eddsa_maps_correctly() {
        assert_eq!(
            key_alg_to_algorithm(&KeyAlgorithm::EdDSA),
            Some(Algorithm::EdDSA)
        );
    }

    #[test]
    fn key_alg_hs256_returns_none() {
        // HS256 in a JWKS (symmetric) has no mapping — caller falls back to infer_alg_from_params
        assert_eq!(key_alg_to_algorithm(&KeyAlgorithm::HS256), None);
    }

    // ── infer_alg_from_params ─────────────────────────────────────────────────

    fn alg_params_from_jwk_json(json: &str) -> AlgorithmParameters {
        let jwk: jsonwebtoken::jwk::Jwk = serde_json::from_str(json).unwrap();
        jwk.algorithm
    }

    #[test]
    fn infer_alg_rsa_defaults_to_rs256() {
        // Minimal RSA public key (RFC 7517 §A.1 excerpt — valid base64url values)
        let params = alg_params_from_jwk_json(
            r#"{"kty":"RSA","n":"0vx7agoebGcQSuuPiLJXZptN9nndrQmbXEps2aiAFbWhM78LhWx4cbbfAAtVT86zwu1RK7aPFFxuhDR1L6tSoc_BJECPebWKRXjBZCiFV4n3oknjhMstn64tZ_2W-5JsGY4Hc5n9yBXArwl93lqt7_RN5w6Cf0h4QyQ5v-65YGjQR0_FDW2QvzqY368QQMicAtaSqzs8KJZgnYb9c7d0zgdAZHzu6qMQvRL5hajrn1n91CbOpbISD08qNLyrdkt-bFTWhAI4vMQFh6WeZu0fM4lFd2NcRwr3XPksINHaQ-G_xBniIqbw0Ls1jF44-csFCur-kEgU8awapJzKnqDKgw","e":"AQAB"}"#,
        );
        assert_eq!(infer_alg_from_params(&params), Algorithm::RS256);
    }

    #[test]
    fn infer_alg_ec_defaults_to_es256() {
        let params = alg_params_from_jwk_json(
            r#"{"kty":"EC","crv":"P-256","x":"f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU","y":"x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0"}"#,
        );
        assert_eq!(infer_alg_from_params(&params), Algorithm::ES256);
    }

    #[test]
    fn infer_alg_oct_defaults_to_hs256() {
        let params = alg_params_from_jwk_json(r#"{"kty":"oct","k":"c2VjcmV0"}"#);
        assert_eq!(infer_alg_from_params(&params), Algorithm::HS256);
    }

    // ── find_key_in_set ───────────────────────────────────────────────────────

    #[test]
    fn find_key_in_empty_set_returns_none() {
        let jwks = JwkSet { keys: vec![] };
        assert!(find_key_in_set(&jwks, None).is_none());
        assert!(find_key_in_set(&jwks, Some("any-kid")).is_none());
    }

    #[test]
    fn find_key_without_kid_returns_first_key() {
        let jwks = oct_jwks(TEST_KID, TEST_SECRET);
        assert!(find_key_in_set(&jwks, None).is_some());
    }

    #[test]
    fn find_key_with_matching_kid_returns_key_and_algorithm() {
        let jwks = oct_jwks(TEST_KID, TEST_SECRET);
        let result = find_key_in_set(&jwks, Some(TEST_KID));
        assert!(result.is_some());
        let (_, alg) = result.unwrap();
        assert_eq!(alg, Algorithm::HS256);
    }

    #[test]
    fn find_key_with_unknown_kid_falls_back_to_first_key() {
        let jwks = oct_jwks(TEST_KID, TEST_SECRET);
        // Unknown kid should fall back to the first key rather than returning None
        let result = find_key_in_set(&jwks, Some("nonexistent-kid"));
        assert!(result.is_some());
    }

    // ── JwksCache ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn cache_returns_none_for_untrusted_issuer() {
        let cache = JwksCache::new(vec![], 300, reqwest::Client::new());
        assert!(cache
            .get_decoding_key("https://untrusted.com", None)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn cache_returns_key_for_seeded_issuer_without_kid() {
        let cache = JwksCache::new(vec![test_issuer_cfg()], 300, reqwest::Client::new());
        cache
            .seed_for_test(
                TEST_ISSUER,
                &format!("{}/jwks.json", TEST_ISSUER),
                oct_jwks(TEST_KID, TEST_SECRET),
            )
            .await;
        assert!(cache.get_decoding_key(TEST_ISSUER, None).await.is_some());
    }

    #[tokio::test]
    async fn cache_returns_key_for_seeded_issuer_with_matching_kid() {
        let cache = JwksCache::new(vec![test_issuer_cfg()], 300, reqwest::Client::new());
        cache
            .seed_for_test(
                TEST_ISSUER,
                &format!("{}/jwks.json", TEST_ISSUER),
                oct_jwks(TEST_KID, TEST_SECRET),
            )
            .await;
        assert!(cache
            .get_decoding_key(TEST_ISSUER, Some(TEST_KID))
            .await
            .is_some());
    }

    #[tokio::test]
    async fn cache_returns_none_when_unseeded_and_server_unreachable() {
        let cache = JwksCache::new(vec![test_issuer_cfg()], 300, reqwest::Client::new());
        // No seed, no server — fetch will fail, stale fallback also empty
        assert!(cache
            .get_decoding_key(TEST_ISSUER, Some(TEST_KID))
            .await
            .is_none());
    }

    #[tokio::test]
    async fn cache_returns_algorithm_from_key_alg_field() {
        let cache = JwksCache::new(vec![test_issuer_cfg()], 300, reqwest::Client::new());
        cache
            .seed_for_test(
                TEST_ISSUER,
                &format!("{}/jwks.json", TEST_ISSUER),
                oct_jwks(TEST_KID, TEST_SECRET),
            )
            .await;
        let (_, alg) = cache
            .get_decoding_key(TEST_ISSUER, Some(TEST_KID))
            .await
            .unwrap();
        assert_eq!(alg, Algorithm::HS256);
    }
}
