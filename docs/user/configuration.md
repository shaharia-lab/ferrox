# Configuration Reference

Ferrox is configured via a single YAML file. The config path is resolved in this order:

1. `LLM_PROXY_CONFIG` environment variable
2. `./config/config.yaml`
3. `/etc/ferrox/config.yaml`

Environment variables can be embedded anywhere in string values using `${VAR}` or `${VAR:-default}` syntax. Substitution is done on the already-parsed YAML value tree, so injected values with special characters are always safe.

## Top-level structure

```yaml
server:               { ... }   # optional; all fields have defaults
telemetry:            { ... }   # optional; all fields have defaults
defaults:             { ... }   # optional; all fields have defaults
providers:            [ ... ]   # required
models:               [ ... ]   # required
virtual_keys:         [ ... ]   # optional; static Bearer keys
trusted_issuers:      [ ... ]   # optional; JWKS-based JWT auth
jwks_cache_ttl_secs:  300       # optional; default 300
rate_limiting:        { ... }   # optional; default: memory backend
usage_database_url:   "..."     # optional; PostgreSQL URL for usage tracking
event_endpoints:      [ ... ]   # optional; webhook push notifications
```

`virtual_keys` and `trusted_issuers` are both optional. You can use one, the other, or both simultaneously.

---

## server

```yaml
server:
  host: "0.0.0.0"          # bind address
  port: 8080                # bind port
  graceful_shutdown_timeout_secs: 30
  max_request_body_bytes: 10485760   # 10 MB

  timeouts:
    connect_secs: 10   # TCP + TLS handshake to provider
    ttfb_secs: 60      # wait for first SSE chunk (reasoning models can be slow)
    idle_secs: 30      # max silence between consecutive SSE chunks
```

All fields have defaults and are optional.

---

## telemetry

```yaml
telemetry:
  log_level: "info"    # trace | debug | info | warn | error
  log_format: "json"   # json | text

  metrics:
    enabled: true
    path: "/metrics"

  tracing:
    enabled: false
    otlp_endpoint: "http://otel-collector:4317"
    service_name: "ferrox"
    service_version: "0.1.0"
    sample_rate: 1.0   # 0.0 = off, 1.0 = 100%
```

---

## defaults

Applied to every provider unless the provider block overrides them.

```yaml
defaults:
  timeouts:
    connect_secs: 10
    ttfb_secs: 60
    idle_secs: 30

  retry:
    max_attempts: 3
    initial_backoff_ms: 100
    max_backoff_ms: 2000
    jitter: true          # adds random jitter up to initial_backoff_ms

  circuit_breaker:
    failure_threshold: 5    # failures before opening
    success_threshold: 2    # successful probes needed to close
    recovery_timeout_secs: 30
```

---

## providers

A list of upstream provider connections. You can have multiple entries of the same type to use multiple API keys or base URLs.

```yaml
providers:
  - name: anthropic-primary        # unique name, referenced by models
    type: anthropic                # anthropic | openai | gemini | bedrock
    api_key: "${ANTHROPIC_API_KEY}"
    base_url: "https://api.anthropic.com"   # optional override

    # Optional per-provider overrides:
    timeouts:
      ttfb_secs: 90
    retry:
      max_attempts: 2
    circuit_breaker:
      failure_threshold: 3
```

For Bedrock, omit `api_key` and set `region`. Credentials come from the AWS credential chain (environment variables, instance role, IRSA).

```yaml
  - name: bedrock-us
    type: bedrock
    region: "${AWS_REGION:-us-east-1}"
```

| Field | Required | Description |
|---|---|---|
| `name` | yes | Unique identifier used in model routing |
| `type` | yes | Provider type: `anthropic`, `openai`, `gemini`, `bedrock` |
| `api_key` | yes* | API key (*not required for Bedrock) |
| `base_url` | no | Override the default endpoint. Must include the API version prefix (e.g. `https://api.openai.com/v1`). The adapter appends only `/chat/completions`. |
| `region` | no | AWS region (Bedrock only) |
| `timeouts` | no | Per-provider timeout overrides |
| `retry` | no | Per-provider retry overrides |
| `circuit_breaker` | no | Per-provider circuit breaker overrides |

---

## models

Each model alias maps client requests to a provider pool with a routing strategy.

```yaml
models:
  - alias: "claude-sonnet"        # the model name clients send in requests
    routing:
      strategy: weighted          # round_robin | weighted | failover | random
      targets:
        - provider: anthropic-primary
          model_id: "claude-sonnet-4-20250514"
          weight: 70              # only required for weighted strategy
        - provider: anthropic-secondary
          model_id: "claude-sonnet-4-20250514"
          weight: 30
      fallback:                   # tried in order when all targets fail
        - provider: bedrock-us
          model_id: "anthropic.claude-3-5-sonnet-20241022-v2:0"
```

| Routing strategy | Behavior |
|---|---|
| `round_robin` | Cycles through available targets in order |
| `weighted` | Distributes traffic proportionally by weight |
| `failover` | Always uses the first available target |
| `random` | Picks a random available target per request |

See [Routing](routing.md) for details on circuit breakers and fallback behavior.

---

## virtual_keys

Virtual keys are the credentials clients use to authenticate with Ferrox. Each key can be scoped to specific models and rate-limited independently.

```yaml
virtual_keys:
  - key: "${PROXY_KEY_APP}"     # the Bearer token clients send
    name: "my-app"              # unique name used in metrics/logs
    description: "Production app"   # optional
    allowed_models:
      - "claude-sonnet"
      - "gpt-4o"
    rate_limit:
      requests_per_minute: 120
      burst: 20
```

Use `allowed_models: ["*"]` to allow access to all model aliases.

See [Virtual Keys](virtual-keys.md) for more detail.

---

## rate_limiting

Controls how rate limit counters are stored. The default in-process memory backend is correct for single-instance deployments. Switch to Redis for accurate enforcement across horizontally scaled replicas.

```yaml
rate_limiting:
  backend: memory          # memory (default) | redis
```

**Redis backend:**

```yaml
rate_limiting:
  backend: redis
  redis_url: "redis://localhost:6379"   # required
  redis_key_prefix: "ferrox:rl:"       # optional; default shown
  redis_pool_size: 10                  # optional; default shown
  redis_fail_open: true                # optional; default shown
```

| Field | Required | Default | Description |
|---|---|---|---|
| `backend` | no | `memory` | Storage backend: `memory` or `redis` |
| `redis_url` | if redis | — | Redis connection URL |
| `redis_key_prefix` | no | `ferrox:rl:` | Key prefix in Redis |
| `redis_pool_size` | no | `10` | Async connection pool size |
| `redis_fail_open` | no | `true` | Allow requests when Redis is unavailable |

### Backend comparison

| | `memory` | `redis` |
|---|---|---|
| Accuracy | Per-instance | Shared across all replicas |
| Latency | Zero overhead | +1 Redis round-trip |
| Availability | Always available | Depends on Redis |
| Config change | None | Requires `redis_url` |

### Redis algorithm

The Redis backend uses a sliding-window counter (sorted set + Lua script, one atomic round-trip per request). This avoids the 2× burst allowed at window boundaries by fixed-window approaches.

### Fail-open behaviour

When `redis_fail_open: true` (default), requests are **allowed** if Redis is unavailable or the Lua script errors. A warning is logged and `ferrox_ratelimit_backend_errors_total{backend="redis"}` is incremented. Set `redis_fail_open: false` to deny requests when Redis is down.

---

## trusted_issuers

Defines external JWT issuers whose tokens Ferrox will accept for authentication. Ferrox fetches each issuer's JWKS, caches the public keys, and validates signatures on incoming JWTs.

```yaml
trusted_issuers:
  - issuer: "https://accounts.google.com"
    jwks_uri: "https://www.googleapis.com/oauth2/v3/certs"
    audience: "my-ferrox-gateway"    # optional

  - issuer: "https://login.microsoftonline.com/<tenant>/v2.0"
    jwks_uri: "https://login.microsoftonline.com/<tenant>/discovery/v2.0/keys"

  - issuer: "https://your-okta-domain.okta.com/oauth2/default"
    jwks_uri: "https://your-okta-domain.okta.com/oauth2/default/v1/keys"
```

| Field | Required | Description |
|---|---|---|
| `issuer` | yes | Expected `iss` claim value — must match exactly |
| `jwks_uri` | yes | URL to fetch the JWKS public keys from |
| `audience` | no | Expected `aud` claim value — omit to skip validation |

### JWT claims

Clients pass a JWT as the Bearer token. Ferrox reads the following custom claims from the token payload:

| Claim | Type | Description |
|---|---|---|
| `ferrox/tenant_id` | string | Used as the rate limit bucket key |
| `ferrox/client_id` | string (UUID) | Client UUID from the control plane `clients` table |
| `ferrox/allowed_models` | `["*"]` or list | Models the token is permitted to use |
| `ferrox/rate_limit.requests_per_minute` | integer | Sustained rate limit |
| `ferrox/rate_limit.burst` | integer | Burst capacity |
| `ferrox/token_budget` | integer | Max tokens per budget period (omitted if unlimited) |
| `ferrox/budget_period` | string | `"daily"` or `"monthly"` (omitted if unlimited) |

All Ferrox-specific claims are optional. If `ferrox/allowed_models` is absent, the token may access all aliases. Budget claims are set automatically when a client has a token budget configured in the control plane.

See [Virtual Keys](virtual-keys.md) for a comparison of static keys vs JWT auth.

---

## usage_database_url

PostgreSQL connection URL for persisting per-request token usage to the `usage_log` table. This should point to the same database used by ferrox-cp.

```yaml
usage_database_url: "${USAGE_DATABASE_URL}"
```

When set, the gateway writes usage records (client, model, provider, prompt/completion tokens, latency) to PostgreSQL via an async batched writer. Records are flushed every 5 seconds or every 100 records, whichever comes first.

When absent, usage recording is silently disabled with zero overhead.

| Feature | Requires `usage_database_url` |
|---|---|
| Per-client usage dashboards in admin UI | yes |
| `GET /api/clients/:id/usage` endpoint | yes |
| Soft budget enforcement (periodic revocation) | yes |
| Real-time budget enforcement (Redis) | no (uses Redis counters) |

---

## event_endpoints

Webhook endpoints that receive async push notifications for per-request token usage. Use this to integrate real-time billing, analytics, or monitoring systems without polling.

```yaml
event_endpoints:
  - name: "billing-webhook"
    url: "${BILLING_WEBHOOK_URL}"
    token: "${BILLING_WEBHOOK_TOKEN}"
    events: ["token_usage"]

  - name: "analytics"
    url: "https://analytics.internal/ingest"
    token: "${ANALYTICS_TOKEN}"
    events: ["token_usage"]
```

| Field | Required | Description |
|---|---|---|
| `name` | yes | Unique identifier (used in logs and Prometheus metrics) |
| `url` | yes | HTTP(S) URL to POST events to |
| `token` | yes | Bearer token sent in the `Authorization` header |
| `events` | yes | Event types to subscribe to (currently: `token_usage`) |

### Delivery behaviour

- **Fully async** — events flow through an internal buffer to a background task. Zero latency added to the request path.
- **Per-endpoint isolation** — each delivery runs independently. A slow or failing endpoint does not block others.
- **Retry with backoff** — 3 attempts (1s → 2s → 4s). On persistent failure, the event is dropped and `ferrox_webhook_errors_total{endpoint}` is incremented.
- **Bounded concurrency** — at most 256 concurrent delivery tasks to prevent resource exhaustion under load.
- **Non-blocking buffer** — if the internal event buffer (10,000 slots) is full, new events are dropped with a warning log.

### Event payload

Each event is sent as an HTTP POST with `Content-Type: application/json` and `Authorization: Bearer <token>`:

```json
{
  "event": "token_usage",
  "request_id": "550e8400-e29b-41d4-a716-446655440000",
  "client_id": "2a2bfb93-99af-414f-99cb-1891435c0806",
  "key_name": "my-app",
  "model": "claude-sonnet",
  "provider": "anthropic-primary",
  "prompt_tokens": 120,
  "completion_tokens": 80,
  "total_tokens": 200,
  "latency_ms": 843,
  "timestamp": "2026-04-06T15:36:12.471Z"
}
```

The `request_id` field is unique per request and can be used for idempotent processing on the receiver side.

### Reliability

Webhooks are best-effort. The `usage_log` database (when `usage_database_url` is configured) remains the durable source of truth. Receivers can reconcile against the usage API for any missed webhook events.

---

## jwks_cache_ttl_secs

How long to cache JWKS public keys (in seconds). Ferrox proactively refreshes keys in the background at 80% of this interval. On refresh failure, the stale cache is served until the next successful refresh.

```yaml
jwks_cache_ttl_secs: 300   # default
```

| Value | Behaviour |
|---|---|
| `300` (default) | Keys are refreshed every ~4 minutes; served stale on failure |
| `3600` | Refresh every ~48 minutes; reduces external calls for stable key sets |
| `60` | Aggressive refresh; use during key rotation events |

---

## Environment variable interpolation

All string values support `${VAR}` and `${VAR:-default}` syntax:

| Syntax | Behavior |
|---|---|
| `${VAR}` | Substitutes the value of `VAR`; errors if unset |
| `${VAR:-default}` | Uses `default` when `VAR` is unset or empty |

Interpolation happens on the already-parsed YAML tree. Values containing YAML special characters (`:`, `{`, `#`, etc.) are safe to inject.
