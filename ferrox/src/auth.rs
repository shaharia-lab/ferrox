use axum::{
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::Deserialize;
use uuid::Uuid;

use crate::config::RateLimitConfig;
use crate::error::ProxyError;
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
    client_id: Option<String>,
    allowed_models: Option<Vec<String>>,
    rate_limit: Option<JwtRateLimitClaims>,
    token_budget: Option<i64>,
    budget_period: Option<String>,
}

#[derive(Deserialize)]
struct JwtRateLimitClaims {
    requests_per_minute: u32,
    burst: u32,
}

// ── Intermediate auth result ──────────────────────────────────────────────────

#[derive(Debug)]
struct AuthOutcome {
    key_name: String,
    allowed_models: Vec<String>,
    client_id: Option<uuid::Uuid>,
    token_budget: Option<i64>,
    budget_period: Option<String>,
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
        validate_static_key(&token, &state).await?
    };

    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    // Pre-request budget reservation (Redis-backed, if configured).
    // Reserves a pessimistic token estimate; handlers reconcile after the response.
    let budget_reserved_tokens = if let (Some(ref client_id), Some(budget), Some(ref period)) = (
        &outcome.client_id,
        outcome.token_budget,
        &outcome.budget_period,
    ) {
        let estimate = crate::budget_enforcer::DEFAULT_RESERVE_TOKENS;
        if state
            .budget_enforcer
            .reserve_tokens(&client_id.to_string(), period, budget, estimate)
            .await
            .is_err()
        {
            return Err(ProxyError::BudgetExceeded(format!(
                "Token budget exceeded for '{}'",
                outcome.key_name
            )));
        }
        estimate
    } else {
        0
    };

    let ctx = RequestContext {
        request_id,
        key_name: outcome.key_name,
        allowed_models: outcome.allowed_models,
        client_id: outcome.client_id,
        token_budget: outcome.token_budget,
        budget_period: outcome.budget_period,
        budget_reserved_tokens,
    };

    req.extensions_mut().insert(ctx);
    Ok(next.run(req).await)
}

// ── Static key path ───────────────────────────────────────────────────────────

async fn validate_static_key(token: &str, state: &AppState) -> Result<AuthOutcome, ProxyError> {
    let key_config = state
        .config
        .virtual_keys
        .iter()
        .find(|k| k.key == token)
        .ok_or_else(|| ProxyError::Unauthorized("Invalid API key".to_string()))?;

    if let Some(rl) = &key_config.rate_limit {
        if state
            .rate_limit_backend
            .check_and_record(&key_config.name, rl)
            .await
            .is_err()
        {
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
        client_id: None, // Static keys have no control-plane identity
        token_budget: None,
        budget_period: None,
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

    // 6. Rate limiting from JWT claims (per tenant_id / sub)
    if let Some(rl) = ferrox.and_then(|f| f.rate_limit.as_ref()) {
        let rl_config = RateLimitConfig {
            requests_per_minute: rl.requests_per_minute,
            burst: rl.burst,
        };
        if state
            .rate_limit_backend
            .check_and_record(&key_name, &rl_config)
            .await
            .is_err()
        {
            RATE_LIMITED_TOTAL.with_label_values(&[&key_name]).inc();
            tracing::warn!(key_name = %key_name, "JWT rate limit exceeded");
            return Err(ProxyError::RateLimited(format!(
                "Rate limit exceeded for '{key_name}'"
            )));
        }
    }

    // Parse client_id UUID from ferrox claims (if present)
    let client_id = ferrox
        .and_then(|f| f.client_id.as_deref())
        .and_then(|id| uuid::Uuid::parse_str(id).ok());

    let token_budget = ferrox.and_then(|f| f.token_budget);
    let budget_period = ferrox.and_then(|f| f.budget_period.clone());

    Ok(AuthOutcome {
        key_name,
        allowed_models,
        client_id,
        token_budget,
        budget_period,
    })
}

// ── Token extraction ──────────────────────────────────────────────────────────

fn extract_bearer_token(req: &Request) -> Result<String, ProxyError> {
    // Accept `x-api-key` first (Anthropic SDK style).
    if let Some(header) = req.headers().get("x-api-key") {
        let token = header
            .to_str()
            .map_err(|_| ProxyError::Unauthorized("Invalid x-api-key encoding".to_string()))?;
        if !token.is_empty() {
            return Ok(token.to_string());
        }
    }

    // Fall back to `Authorization: Bearer <token>` (OpenAI SDK style).
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

    fn build_request_with_api_key(api_key: Option<&str>) -> HttpRequest<Body> {
        let mut builder = HttpRequest::builder().uri("/");
        if let Some(value) = api_key {
            builder = builder.header("x-api-key", value);
        }
        builder.body(Body::empty()).unwrap()
    }

    // ── x-api-key header ──────────────────────────────────────────────────────

    #[test]
    fn x_api_key_header_is_accepted() {
        let req = build_request_with_api_key(Some("sk-ant-my-key"));
        let token = extract_bearer_token(&req).unwrap();
        assert_eq!(token, "sk-ant-my-key");
    }

    #[test]
    fn x_api_key_takes_precedence_over_authorization() {
        let req = HttpRequest::builder()
            .uri("/")
            .header("x-api-key", "key-from-x-api-key")
            .header("Authorization", "Bearer key-from-bearer")
            .body(Body::empty())
            .unwrap();
        let token = extract_bearer_token(&req).unwrap();
        assert_eq!(token, "key-from-x-api-key");
    }

    #[test]
    fn empty_x_api_key_falls_through_to_authorization() {
        let req = HttpRequest::builder()
            .uri("/")
            .header("x-api-key", "")
            .header("Authorization", "Bearer fallback-key")
            .body(Body::empty())
            .unwrap();
        let token = extract_bearer_token(&req).unwrap();
        assert_eq!(token, "fallback-key");
    }

    #[test]
    fn missing_both_headers_returns_unauthorized() {
        let req = build_request_with_api_key(None);
        let err = extract_bearer_token(&req).unwrap_err();
        assert!(matches!(err, ProxyError::Unauthorized(_)));
    }

    // ── Authorization: Bearer header ──────────────────────────────────────────

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

    #[test]
    fn peek_iss_returns_none_when_iss_missing_from_payload() {
        let payload = r#"{"sub":"only-sub","exp":9999999999}"#;
        let encoded = URL_SAFE_NO_PAD.encode(payload.as_bytes());
        let fake_jwt = format!("header.{}.sig", encoded);
        assert_eq!(peek_iss(&fake_jwt), None);
    }

    #[test]
    fn looks_like_jwt_with_exactly_two_dots() {
        assert!(looks_like_jwt("a.b.c"));
    }

    #[test]
    fn looks_like_jwt_false_with_one_dot() {
        assert!(!looks_like_jwt("a.b"));
    }

    #[test]
    fn looks_like_jwt_false_with_three_dots() {
        assert!(!looks_like_jwt("a.b.c.d"));
    }

    #[test]
    fn looks_like_jwt_false_for_empty_string() {
        assert!(!looks_like_jwt(""));
    }

    // ── validate_static_key ───────────────────────────────────────────────────

    mod static_key_tests {
        use super::*;
        use std::collections::HashMap;
        use std::sync::{atomic::AtomicBool, Arc};

        use crate::config::{
            Config, DefaultsConfig, RateLimitConfig, RateLimitingConfig, ServerConfig,
            TelemetryConfig, VirtualKeyConfig,
        };
        use crate::jwks::JwksCache;
        use crate::metrics::Metrics;
        use crate::providers::ProviderRegistry;
        use crate::ratelimit::MemoryBackend;
        use crate::router::ModelRouter;

        fn config_with_key(key_str: &str, rpm: Option<u32>) -> Config {
            Config {
                server: ServerConfig::default(),
                telemetry: TelemetryConfig::default(),
                defaults: DefaultsConfig::default(),
                providers: vec![],
                models: vec![],
                virtual_keys: vec![VirtualKeyConfig {
                    key: key_str.to_string(),
                    name: "test-key".to_string(),
                    description: None,
                    allowed_models: vec!["claude-sonnet".to_string()],
                    rate_limit: rpm.map(|r| RateLimitConfig {
                        requests_per_minute: r,
                        burst: 1,
                    }),
                }],
                trusted_issuers: vec![],
                jwks_cache_ttl_secs: 300,
                rate_limiting: RateLimitingConfig::default(),
                usage_database_url: None,
                event_endpoints: vec![],
            }
        }

        fn build_state(config: Config) -> AppState {
            let registry: ProviderRegistry = HashMap::new();
            let router = ModelRouter::from_config(&config, &registry).unwrap();
            let jwks_cache = JwksCache::new(vec![], 300, reqwest::Client::new());
            AppState {
                rate_limit_backend: Arc::new(MemoryBackend::new()),
                router: Arc::new(router),
                providers: Arc::new(registry),
                metrics: Arc::new(Metrics::new()),
                ready: Arc::new(AtomicBool::new(true)),
                jwks_cache: Arc::new(jwks_cache),
                config: Arc::new(config),
                usage_writer: crate::usage_writer::noop_writer(),
                budget_enforcer: Arc::new(crate::budget_enforcer::NoopBudgetEnforcer),
                event_dispatcher: crate::event_dispatcher::noop_dispatcher(),
            }
        }

        #[tokio::test]
        async fn unknown_static_key_returns_unauthorized() {
            let state = build_state(config_with_key("sk-real", None));
            let err = validate_static_key("sk-wrong", &state).await.unwrap_err();
            assert!(matches!(err, ProxyError::Unauthorized(_)));
        }

        #[tokio::test]
        async fn valid_static_key_returns_correct_outcome() {
            let state = build_state(config_with_key("sk-real", None));
            let outcome = validate_static_key("sk-real", &state).await.unwrap();
            assert_eq!(outcome.key_name, "test-key");
            assert_eq!(outcome.allowed_models, vec!["claude-sonnet"]);
        }

        #[tokio::test]
        async fn rate_limited_static_key_returns_rate_limited_error() {
            // burst = 1, so the second request is denied
            let state = build_state(config_with_key("sk-real", Some(60)));
            let _ = validate_static_key("sk-real", &state).await.unwrap(); // first: ok
            let err = validate_static_key("sk-real", &state).await.unwrap_err();
            assert!(matches!(err, ProxyError::RateLimited(_)));
        }
    }

    // ── validate_jwt ──────────────────────────────────────────────────────────

    mod jwt_tests {
        use super::*;
        use std::collections::HashMap;
        use std::sync::{atomic::AtomicBool, Arc};

        use crate::config::{
            Config, DefaultsConfig, RateLimitingConfig, ServerConfig, TelemetryConfig,
            TrustedIssuerConfig,
        };
        use crate::jwks::JwksCache;
        use crate::metrics::Metrics;
        use crate::providers::ProviderRegistry;
        use crate::ratelimit::MemoryBackend;
        use crate::router::ModelRouter;

        const SECRET: &[u8] = b"test-secret-for-jwt-auth-tests";
        const KID: &str = "test-kid";
        const ISSUER: &str = "https://test.example.com";

        fn oct_jwks(kid: &str, secret: &[u8]) -> jsonwebtoken::jwk::JwkSet {
            let k = URL_SAFE_NO_PAD.encode(secret);
            serde_json::from_str(&format!(
                r#"{{"keys":[{{"kty":"oct","kid":"{}","alg":"HS256","k":"{}"}}]}}"#,
                kid, k
            ))
            .unwrap()
        }

        fn make_jwt(claims: serde_json::Value, secret: &[u8], kid: &str) -> String {
            let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
            header.kid = Some(kid.to_string());
            jsonwebtoken::encode(
                &header,
                &claims,
                &jsonwebtoken::EncodingKey::from_secret(secret),
            )
            .unwrap()
        }

        fn valid_claims() -> serde_json::Value {
            serde_json::json!({
                "sub": "test-service",
                "iss": ISSUER,
                "exp": 9_999_999_999u64,
            })
        }

        fn config_with_issuer(issuer: &str, audience: Option<&str>) -> Config {
            Config {
                server: ServerConfig::default(),
                telemetry: TelemetryConfig::default(),
                defaults: DefaultsConfig::default(),
                providers: vec![],
                models: vec![],
                virtual_keys: vec![],
                trusted_issuers: vec![TrustedIssuerConfig {
                    issuer: issuer.to_string(),
                    jwks_uri: format!("{}/jwks.json", issuer),
                    audience: audience.map(str::to_string),
                }],
                jwks_cache_ttl_secs: 300,
                rate_limiting: RateLimitingConfig::default(),
                usage_database_url: None,
                event_endpoints: vec![],
            }
        }

        async fn build_state(config: Config) -> AppState {
            let registry: ProviderRegistry = HashMap::new();
            let router = ModelRouter::from_config(&config, &registry).unwrap();
            let jwks_cache = JwksCache::new(
                config.trusted_issuers.clone(),
                config.jwks_cache_ttl_secs,
                reqwest::Client::new(),
            );
            jwks_cache
                .seed_for_test(
                    ISSUER,
                    &format!("{}/jwks.json", ISSUER),
                    oct_jwks(KID, SECRET),
                )
                .await;
            AppState {
                rate_limit_backend: Arc::new(MemoryBackend::new()),
                router: Arc::new(router),
                providers: Arc::new(registry),
                metrics: Arc::new(Metrics::new()),
                ready: Arc::new(AtomicBool::new(true)),
                jwks_cache: Arc::new(jwks_cache),
                config: Arc::new(config),
                usage_writer: crate::usage_writer::noop_writer(),
                budget_enforcer: Arc::new(crate::budget_enforcer::NoopBudgetEnforcer),
                event_dispatcher: crate::event_dispatcher::noop_dispatcher(),
            }
        }

        #[tokio::test]
        async fn untrusted_issuer_returns_unauthorized() {
            let state = build_state(config_with_issuer(ISSUER, None)).await;
            let token = make_jwt(
                serde_json::json!({"sub":"s","iss":"https://evil.com","exp":9_999_999_999u64}),
                SECRET,
                KID,
            );
            let err = validate_jwt(&token, &state).await.unwrap_err();
            assert!(matches!(err, ProxyError::Unauthorized(_)));
        }

        #[tokio::test]
        async fn expired_jwt_returns_unauthorized() {
            let state = build_state(config_with_issuer(ISSUER, None)).await;
            let token = make_jwt(
                serde_json::json!({"sub":"s","iss":ISSUER,"exp":1u64}), // exp in the past
                SECRET,
                KID,
            );
            let err = validate_jwt(&token, &state).await.unwrap_err();
            assert!(matches!(err, ProxyError::Unauthorized(_)));
        }

        #[tokio::test]
        async fn tampered_signature_returns_unauthorized() {
            let state = build_state(config_with_issuer(ISSUER, None)).await;
            let token = make_jwt(valid_claims(), SECRET, KID);
            // Append garbage to break the signature
            let tampered = format!("{}xyz", token);
            let err = validate_jwt(&tampered, &state).await.unwrap_err();
            assert!(matches!(err, ProxyError::Unauthorized(_)));
        }

        #[tokio::test]
        async fn wrong_secret_returns_unauthorized() {
            let state = build_state(config_with_issuer(ISSUER, None)).await;
            // Signed with a different secret than what the JWKS cache has
            let token = make_jwt(valid_claims(), b"completely-different-secret", KID);
            let err = validate_jwt(&token, &state).await.unwrap_err();
            assert!(matches!(err, ProxyError::Unauthorized(_)));
        }

        #[tokio::test]
        async fn valid_jwt_without_ferrox_claims_falls_back_to_sub() {
            let state = build_state(config_with_issuer(ISSUER, None)).await;
            let token = make_jwt(valid_claims(), SECRET, KID);
            let outcome = validate_jwt(&token, &state).await.unwrap();
            assert_eq!(outcome.key_name, "test-service"); // from sub
            assert_eq!(outcome.allowed_models, vec!["*"]); // default
        }

        #[tokio::test]
        async fn valid_jwt_with_ferrox_tenant_id_uses_tenant_id() {
            let state = build_state(config_with_issuer(ISSUER, None)).await;
            let token = make_jwt(
                serde_json::json!({
                    "sub": "test-service",
                    "iss": ISSUER,
                    "exp": 9_999_999_999u64,
                    "ferrox": { "tenant_id": "payments-team" }
                }),
                SECRET,
                KID,
            );
            let outcome = validate_jwt(&token, &state).await.unwrap();
            assert_eq!(outcome.key_name, "payments-team");
        }

        #[tokio::test]
        async fn valid_jwt_with_allowed_models_restricts_access() {
            let state = build_state(config_with_issuer(ISSUER, None)).await;
            let token = make_jwt(
                serde_json::json!({
                    "sub": "s",
                    "iss": ISSUER,
                    "exp": 9_999_999_999u64,
                    "ferrox": { "allowed_models": ["claude-sonnet", "gpt-4o"] }
                }),
                SECRET,
                KID,
            );
            let outcome = validate_jwt(&token, &state).await.unwrap();
            assert_eq!(outcome.allowed_models, vec!["claude-sonnet", "gpt-4o"]);
        }

        #[tokio::test]
        async fn valid_jwt_with_rate_limit_enforces_burst() {
            let state = build_state(config_with_issuer(ISSUER, None)).await;
            let make_token = || {
                make_jwt(
                    serde_json::json!({
                        "sub": "s",
                        "iss": ISSUER,
                        "exp": 9_999_999_999u64,
                        "ferrox": { "rate_limit": { "requests_per_minute": 60, "burst": 1 } }
                    }),
                    SECRET,
                    KID,
                )
            };
            // First request: allowed
            assert!(validate_jwt(&make_token(), &state).await.is_ok());
            // Second request: bucket exhausted (burst = 1)
            let err = validate_jwt(&make_token(), &state).await.unwrap_err();
            assert!(matches!(err, ProxyError::RateLimited(_)));
        }

        #[tokio::test]
        async fn jwt_with_correct_audience_passes_validation() {
            let state = build_state(config_with_issuer(ISSUER, Some("ferrox"))).await;
            let token = make_jwt(
                serde_json::json!({
                    "sub": "s",
                    "iss": ISSUER,
                    "aud": "ferrox",
                    "exp": 9_999_999_999u64
                }),
                SECRET,
                KID,
            );
            assert!(validate_jwt(&token, &state).await.is_ok());
        }

        #[tokio::test]
        async fn jwt_with_wrong_audience_returns_unauthorized() {
            let state = build_state(config_with_issuer(ISSUER, Some("ferrox"))).await;
            let token = make_jwt(
                serde_json::json!({
                    "sub": "s",
                    "iss": ISSUER,
                    "aud": "some-other-service",
                    "exp": 9_999_999_999u64
                }),
                SECRET,
                KID,
            );
            let err = validate_jwt(&token, &state).await.unwrap_err();
            assert!(matches!(err, ProxyError::Unauthorized(_)));
        }
    }
}
