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
