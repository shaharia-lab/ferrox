use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::Deserialize;
use uuid::Uuid;

use crate::error::ProxyError;
use crate::ratelimit::token_bucket::TokenBucket;
use crate::state::AppState;
use crate::telemetry::metrics::RATE_LIMITED_TOTAL;
use crate::types::RequestContext;

// ── JWT claim structs ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PeekClaims {
    iss: String,
}

#[derive(Deserialize)]
struct FerroxJwtClaims {
    sub: String,
    #[serde(default)]
    ferrox: Option<FerroxCustomClaims>,
}

#[derive(Deserialize)]
struct FerroxCustomClaims {
    tenant_id: Option<String>,
    allowed_models: Option<Vec<String>>,
    rate_limit: Option<JwtRateLimitClaims>,
}

#[derive(Deserialize)]
struct JwtRateLimitClaims {
    requests_per_minute: u32,
    burst: u32,
}

// ── Intermediate auth result ──────────────────────────────────────────────────

struct AuthOutcome {
    key_name: String,
    allowed_models: Vec<String>,
}

// ── Middleware ────────────────────────────────────────────────────────────────

pub async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, ProxyError> {
    let token = extract_bearer_token(&req)?;

    let outcome = if looks_like_jwt(&token) {
        validate_jwt(&token, &state).await?
    } else {
        validate_static_key(&token, &state)?
    };

    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let ctx = RequestContext {
        request_id,
        key_name: outcome.key_name,
        allowed_models: outcome.allowed_models,
    };

    req.extensions_mut().insert(ctx);
    Ok(next.run(req).await)
}

// ── Static key path ───────────────────────────────────────────────────────────

fn validate_static_key(token: &str, state: &AppState) -> Result<AuthOutcome, ProxyError> {
    let key_config = state
        .config
        .virtual_keys
        .iter()
        .find(|k| k.key == token)
        .ok_or_else(|| ProxyError::Unauthorized("Invalid API key".to_string()))?;

    if let Some(bucket) = state.rate_limiter.get(&key_config.name) {
        if !bucket.try_consume() {
            RATE_LIMITED_TOTAL
                .with_label_values(&[&key_config.name])
                .inc();
            tracing::warn!(key_name = %key_config.name, "Rate limit exceeded");
            return Err(ProxyError::RateLimited(format!(
                "Rate limit exceeded for key '{}'",
                key_config.name
            )));
        }
    }

    Ok(AuthOutcome {
        key_name: key_config.name.clone(),
        allowed_models: key_config.allowed_models.clone(),
    })
}

// ── JWT path ──────────────────────────────────────────────────────────────────

fn looks_like_jwt(token: &str) -> bool {
    token.chars().filter(|&c| c == '.').count() == 2
}

/// Peek at the `iss` claim without verifying the signature.
/// Safe because we only use `iss` to look up the trusted issuer config;
/// full signature validation happens immediately after.
fn peek_iss(token: &str) -> Option<String> {
    let payload_b64 = token.split('.').nth(1)?;
    let payload_bytes = URL_SAFE_NO_PAD.decode(payload_b64).ok()?;
    let claims: PeekClaims = serde_json::from_slice(&payload_bytes).ok()?;
    Some(claims.iss)
}

async fn validate_jwt(token: &str, state: &AppState) -> Result<AuthOutcome, ProxyError> {
    // 1. Decode header (unauthenticated — only used for kid lookup)
    let header = jsonwebtoken::decode_header(token)
        .map_err(|_| ProxyError::Unauthorized("Invalid token".to_string()))?;

    // 2. Peek at iss to find the trusted issuer config
    let iss =
        peek_iss(token).ok_or_else(|| ProxyError::Unauthorized("Invalid token".to_string()))?;

    let issuer_cfg = state
        .config
        .trusted_issuers
        .iter()
        .find(|i| i.issuer == iss)
        .ok_or_else(|| ProxyError::Unauthorized("Invalid token".to_string()))?;

    // 3. Get decoding key from JWKS cache (refreshes if needed)
    let (decoding_key, alg) = state
        .jwks_cache
        .get_decoding_key(&iss, header.kid.as_deref())
        .await
        .ok_or_else(|| ProxyError::Unauthorized("Invalid token".to_string()))?;

    // 4. Full JWT validation (signature + expiry + issuer + optional audience)
    let mut validation = jsonwebtoken::Validation::new(alg);
    validation.set_issuer(&[&iss]);
    match &issuer_cfg.audience {
        Some(aud) => validation.set_audience(&[aud]),
        None => {
            validation.validate_aud = false;
        }
    }

    let token_data: jsonwebtoken::TokenData<FerroxJwtClaims> =
        jsonwebtoken::decode(token, &decoding_key, &validation)
            .map_err(|_| ProxyError::Unauthorized("Invalid token".to_string()))?;

    // 5. Extract ferrox claims
    let claims = token_data.claims;
    let ferrox = claims.ferrox.as_ref();

    let key_name = ferrox
        .and_then(|f| f.tenant_id.as_deref())
        .unwrap_or(&claims.sub)
        .to_string();

    let allowed_models = ferrox
        .and_then(|f| f.allowed_models.clone())
        .unwrap_or_else(|| vec!["*".to_string()]);

    // 6. In-process rate limiting from JWT claims (per tenant_id / sub)
    if let Some(rl) = ferrox.and_then(|f| f.rate_limit.as_ref()) {
        let bucket = get_or_create_jwt_bucket(
            &state.jwt_rate_limiters,
            &key_name,
            rl.requests_per_minute,
            rl.burst,
        );
        if !bucket.try_consume() {
            RATE_LIMITED_TOTAL.with_label_values(&[&key_name]).inc();
            tracing::warn!(key_name = %key_name, "JWT rate limit exceeded");
            return Err(ProxyError::RateLimited(format!(
                "Rate limit exceeded for '{key_name}'"
            )));
        }
    }

    Ok(AuthOutcome {
        key_name,
        allowed_models,
    })
}

fn get_or_create_jwt_bucket(
    limiters: &Arc<RwLock<HashMap<String, Arc<TokenBucket>>>>,
    key: &str,
    rpm: u32,
    burst: u32,
) -> Arc<TokenBucket> {
    // Fast path: bucket already exists
    {
        let map = limiters.read().unwrap();
        if let Some(b) = map.get(key) {
            return b.clone();
        }
    }
    // Slow path: create and insert (guard against concurrent inserts with entry API)
    let mut map = limiters.write().unwrap();
    map.entry(key.to_string())
        .or_insert_with(|| Arc::new(TokenBucket::new(rpm, burst)))
        .clone()
}

// ── Token extraction ──────────────────────────────────────────────────────────

fn extract_bearer_token(req: &Request) -> Result<String, ProxyError> {
    let header = req
        .headers()
        .get("Authorization")
        .ok_or_else(|| ProxyError::Unauthorized("Missing Authorization header".to_string()))?;

    let value = header.to_str().map_err(|_| {
        ProxyError::Unauthorized("Invalid Authorization header encoding".to_string())
    })?;

    let token = value.strip_prefix("Bearer ").ok_or_else(|| {
        ProxyError::Unauthorized("Authorization header must use Bearer scheme".to_string())
    })?;

    if token.is_empty() {
        return Err(ProxyError::Unauthorized("Empty bearer token".to_string()));
    }

    Ok(token.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;

    fn build_request(auth_header: Option<&str>) -> HttpRequest<Body> {
        let mut builder = HttpRequest::builder().uri("/");
        if let Some(value) = auth_header {
            builder = builder.header("Authorization", value);
        }
        builder.body(Body::empty()).unwrap()
    }

    #[test]
    fn missing_auth_header_returns_unauthorized() {
        let req = build_request(None);
        let err = extract_bearer_token(&req).unwrap_err();
        assert!(matches!(err, ProxyError::Unauthorized(_)));
        assert!(err.to_string().contains("Missing Authorization header"));
    }

    #[test]
    fn non_bearer_scheme_returns_unauthorized() {
        let req = build_request(Some("Basic dXNlcjpwYXNz"));
        let err = extract_bearer_token(&req).unwrap_err();
        assert!(matches!(err, ProxyError::Unauthorized(_)));
        assert!(err.to_string().contains("Bearer scheme"));
    }

    #[test]
    fn empty_token_returns_unauthorized() {
        let req = build_request(Some("Bearer "));
        let err = extract_bearer_token(&req).unwrap_err();
        assert!(matches!(err, ProxyError::Unauthorized(_)));
        assert!(err.to_string().contains("Empty bearer token"));
    }

    #[test]
    fn valid_bearer_token_is_extracted() {
        let req = build_request(Some("Bearer sk-my-secret-key"));
        let token = extract_bearer_token(&req).unwrap();
        assert_eq!(token, "sk-my-secret-key");
    }

    #[test]
    fn looks_like_jwt_with_three_parts() {
        assert!(looks_like_jwt("aaa.bbb.ccc"));
    }

    #[test]
    fn looks_like_jwt_false_for_static_key() {
        assert!(!looks_like_jwt("sk-local-dev"));
        assert!(!looks_like_jwt("sk-ant-abc123"));
    }

    #[test]
    fn peek_iss_extracts_issuer() {
        // Build a minimal JWT payload: {"iss":"https://example.com","sub":"test"}
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
        let payload = r#"{"iss":"https://example.com","sub":"test","exp":9999999999}"#;
        let encoded = URL_SAFE_NO_PAD.encode(payload.as_bytes());
        let fake_jwt = format!("header.{}.sig", encoded);
        assert_eq!(peek_iss(&fake_jwt), Some("https://example.com".to_string()));
    }

    #[test]
    fn peek_iss_returns_none_for_invalid_base64() {
        assert_eq!(peek_iss("header.!!!.sig"), None);
    }

    #[test]
    fn peek_iss_returns_none_for_non_jwt() {
        assert_eq!(peek_iss("sk-local-dev"), None);
    }
}
