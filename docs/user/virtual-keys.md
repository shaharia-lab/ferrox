# Virtual Keys and Authentication

Ferrox supports two authentication mechanisms. Both use the standard HTTP Bearer token header and can be used simultaneously.

| Method | How it works | Best for |
|---|---|---|
| **Static virtual keys** | Pre-shared opaque strings defined in `virtual_keys` config | Simple setups, self-hosted teams, CI/CD |
| **JWKS JWT** | Signed JWT validated against a trusted issuer's public keys | Enterprise IdP (Okta, Azure AD, Google), microservices with existing identity |

---

## Static virtual keys

Virtual keys are pre-shared opaque strings configured in YAML. Each key can be scoped to specific models and rate-limited independently.

Ferrox itself does not call upstream providers with these keys. Each virtual key maps to one or more upstream provider API keys through the routing config.

## Configuration

```yaml
virtual_keys:
  - key: "${PROXY_KEY_APP}"     # Bearer token clients send in Authorization header
    name: "my-app"              # unique name; appears in logs and metrics
    description: "Production API"
    allowed_models:
      - "claude-sonnet"
      - "gpt-4o"
    rate_limit:
      requests_per_minute: 120
      burst: 20
```

## Authentication

Clients authenticate with a standard HTTP Bearer token:

```
Authorization: Bearer <virtual-key>
```

Requests without this header, or with an unrecognized key, receive a `401 Unauthorized` response.

## Model access control

`allowed_models` is a list of model aliases the key is permitted to use. Set to `["*"]` to allow all configured aliases.

```yaml
allowed_models: ["*"]                         # all aliases
allowed_models: ["claude-sonnet", "gpt-4o"]   # specific aliases only
```

Requests to a model alias not in the list receive a `403 Forbidden` response.

## Rate limiting

Ferrox uses a lock-free token bucket per key, per instance. It is approximate for multi-instance deployments (each instance maintains its own bucket independently).

```yaml
rate_limit:
  requests_per_minute: 120   # sustained throughput
  burst: 20                  # max instantaneous burst
```

When the bucket is empty, the request is rejected immediately with `429 Too Many Requests`.

The `burst` value sets the bucket capacity. A fully-charged bucket allows `burst` requests instantly before the sustained limit applies.

### Disabling rate limiting

Omit the `rate_limit` field to remove limits for a key:

```yaml
virtual_keys:
  - key: "${PROXY_KEY_INTERNAL}"
    name: "internal-batch-job"
    allowed_models: ["*"]
    # no rate_limit
```

## Example: multi-tenant setup

```yaml
virtual_keys:
  # Internal service: unrestricted access
  - key: "${KEY_INTERNAL}"
    name: "data-pipeline"
    allowed_models: ["*"]

  # Customer A: claude only, 60 rpm
  - key: "${KEY_CUSTOMER_A}"
    name: "customer-a"
    allowed_models: ["claude-sonnet", "claude-haiku"]
    rate_limit:
      requests_per_minute: 60
      burst: 10

  # Customer B: limited to cheap models, 30 rpm
  - key: "${KEY_CUSTOMER_B}"
    name: "customer-b"
    allowed_models: ["claude-haiku", "gemini-flash"]
    rate_limit:
      requests_per_minute: 30
      burst: 5
```

---

## JWT authentication

For teams with an existing identity provider (Okta, Azure AD, Google, Auth0), Ferrox can validate JWTs directly. No pre-shared key distribution required.

### Setup

1. Add one or more `trusted_issuers` entries pointing to your IdP's JWKS URI:

```yaml
trusted_issuers:
  - issuer: "https://your-okta-domain.okta.com/oauth2/default"
    jwks_uri: "https://your-okta-domain.okta.com/oauth2/default/v1/keys"
    audience: "my-ferrox-gateway"   # optional but recommended
```

2. Clients present the JWT as a Bearer token — no changes to the Authorization header format:

```
Authorization: Bearer eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9...
```

### Custom claims

Ferrox reads the following optional custom claims from the JWT payload to control access and rate limiting:

```json
{
  "iss": "https://your-okta-domain.okta.com/oauth2/default",
  "sub": "service-account-123",
  "ferrox/tenant_id": "team-backend",
  "ferrox/allowed_models": ["claude-sonnet", "gpt-4o"],
  "ferrox/rate_limit": {
    "requests_per_minute": 120,
    "burst": 20
  }
}
```

| Claim | Type | Default if absent |
|---|---|---|
| `ferrox/tenant_id` | string | Uses `sub` as bucket key |
| `ferrox/allowed_models` | list or `["*"]` | All aliases allowed |
| `ferrox/rate_limit.requests_per_minute` | integer | No rate limit |
| `ferrox/rate_limit.burst` | integer | No rate limit |

### Key rotation

Ferrox caches JWKS keys and refreshes them in the background (see `jwks_cache_ttl_secs`). During a key rotation:
- New keys are picked up automatically within one TTL cycle (default: 5 minutes).
- If a kid is not found in the current cache, Ferrox immediately fetches fresh keys before failing the request.
- On refresh failure, the stale cache is served so existing valid tokens continue to work.

See [Configuration](configuration.md#trusted_issuers) for the full reference.

---

## Metrics

Ferrox records the following metrics per key:

- `ferrox_requests_total{key_name=...}` - requests dispatched
- `ferrox_rate_limited_total{key_name=...}` - requests rejected by rate limiter

See [Observability](observability.md) for the full metrics reference.
