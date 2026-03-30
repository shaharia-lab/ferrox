# Architecture

## Overview

Ferrox is a stateless HTTP proxy. Every request is self-contained; no session state is shared between instances. This makes it trivially horizontally scalable.

```mermaid
flowchart TD
    Client -->|POST /v1/chat/completions| Axum

    subgraph Ferrox["Ferrox (single binary)"]
        Axum[axum HTTP server]
        Auth[auth middleware\nBearer token — static key or JWT]
        Router[ModelRouter\nalias -> RoutePool]
        Dispatch[dispatch\nprimary targets + fallback chain]
        CB[CircuitBreaker\nper provider+model]
        Retry[execute_with_retry\nexponential backoff + jitter]

        Axum --> Auth
        Auth --> Router
        Router --> Dispatch
        Dispatch --> CB
        CB --> Retry
    end

    Retry -->|HTTP| Anthropic
    Retry -->|HTTP| OpenAI
    Retry -->|HTTP| Gemini
    Retry -->|AWS SDK| Bedrock

    Ferrox -->|Prometheus text| Scraper[Prometheus scraper]
    Ferrox -->|OTLP gRPC| Collector[OTEL Collector]
```

## Repository layout

This is a Cargo workspace with two crates:

- **`ferrox/`** — the gateway binary (this document describes its internals)
- **`ferrox-cp/`** — the control plane binary (Phase 3, in progress)

## Control plane (`ferrox-cp`)

The control plane manages JWT signing keys, API clients, and token issuance. It is a separate binary in the workspace (`ferrox-cp/`) that connects to a PostgreSQL database.

### Data layer

Three tables form the persistence model:

| Table | Purpose |
|---|---|
| `clients` | API client registrations — name, bcrypt-hashed key, allowed models, rate-limit settings |
| `signing_keys` | RS256 keypairs — private key is AES-256-GCM encrypted at rest; public key is DER-encoded SubjectPublicKeyInfo |
| `audit_log` | Append-only event log — `client_created`, `token_issued`, `key_rotated`, `client_revoked` |

Migrations are embedded in the binary at compile time via `sqlx::migrate!("./migrations")` and applied at startup. The `MIGRATOR` static is `pub` so integration tests can reference it with `#[sqlx::test(migrator = "crate::MIGRATOR")]`.

### Repository pattern

Each table has a typed repository struct (`ClientRepository`, `SigningKeyRepository`, `AuditRepository`). All SQL is written with runtime queries (`sqlx::query_as::<_, T>(sql).bind(...)`) rather than compile-time macros so the binary compiles without a live database. Repositories borrow a `&PgPool` and are cheap to construct per request.

```
ferrox-cp/src/
  main.rs             startup, MIGRATOR static
  config.rs           CpConfig loaded from env vars
  state.rs            CpState (db pool + config, Arc-wrapped)

  db/
    mod.rs            re-exports all repo types
    models.rs         Client, SigningKey, AuditEntry, AuditEvent
    error.rs          RepoError (Conflict / NotFound / Database)
    client_repo.rs    CRUD + revoke + paginate for clients
    signing_key_repo.rs  create / get_active / retire for signing keys
    audit_repo.rs     record / list / count_tokens_issued

ferrox-cp/migrations/
  20240001000000_initial_schema.sql
```

### Environment variables

| Variable | Required | Default | Description |
|---|---|---|---|
| `DATABASE_URL` | yes | — | PostgreSQL connection string |
| `CP_ENCRYPTION_KEY` | yes | — | 64 hex chars (32 bytes) — AES-256-GCM key for private keys at rest |
| `CP_ADMIN_KEY` | yes | — | Static bearer token protecting all admin endpoints |
| `CP_ISSUER` | no | `https://ferrox-cp` | `iss` claim in signed JWTs |
| `CP_PORT` | no | `9090` | TCP port for the control-plane HTTP server |

---

## Gateway module map

```
ferrox/src/
  main.rs             startup, graceful shutdown
  server.rs           axum router, middleware stack
  config.rs           YAML loading, env var interpolation, validation
  state.rs            AppState (shared, Arc-wrapped)
  auth.rs             Bearer token auth: static virtual key or JWKS-validated JWT
  jwks.rs             JWKS cache: fetch, TTL refresh, stale fallback, background task
  router.rs           ModelRouter: alias -> Arc<RoutePool>
  error.rs            ProxyError enum, OpenAI-format HTTP responses
  types.rs            OpenAI wire types (request, response, chunk)
  retry.rs            execute_with_retry, is_retryable, backoff_duration
  metrics.rs          thin shim: initialises telemetry::metrics at startup

  providers/
    mod.rs            ProviderAdapter trait, ProviderRegistry, parse_sse_stream
    anthropic.rs      Anthropic Messages API adapter
    openai.rs         OpenAI Chat Completions adapter
    gemini.rs         Gemini generateContent adapter
    bedrock.rs        AWS Bedrock invoke_model adapter

  lb/
    mod.rs            RoutePool, RouteTarget, select_target
    strategy.rs       LbStrategy: RoundRobin, Weighted, Failover, Random
    circuit_breaker.rs  lock-free CircuitBreaker (AtomicU8 state, AtomicU32 counters)

  ratelimit/
    mod.rs            re-exports: RateLimitBackend trait, MemoryBackend, RedisBackend
    backend.rs        RateLimitBackend async trait
    memory.rs         MemoryBackend: lock-free per-instance token buckets (default)
    redis_backend.rs  RedisBackend: sliding-window Lua script via deadpool-redis
    token_bucket.rs   lock-free TokenBucket (AtomicU64 milli-tokens, CAS loop)

  telemetry/
    mod.rs            init_logging (tracing-subscriber stack)
    metrics.rs        Prometheus Lazy<CounterVec/HistogramVec/GaugeVec> statics
    otel.rs           OTLP tracer initialisation and shutdown

  handlers/
    mod.rs
    chat.rs           chat_completions handler, dispatch_non_stream, dispatch_stream
    health.rs         /healthz, /readyz
    models.rs         /v1/models
```

---

## Request lifecycle

```mermaid
sequenceDiagram
    participant C as Client
    participant A as Auth Middleware
    participant JWKS as JwksCache
    participant R as ModelRouter
    participant P as RoutePool
    participant CB as CircuitBreaker
    participant Up as Upstream Provider

    C->>A: POST /v1/chat/completions<br/>Authorization: Bearer <token>

    alt token looks like a JWT (two dots)
        A->>A: peek issuer (base64 decode payload, no sig check)
        A->>JWKS: get_decoding_key(issuer, kid)
        JWKS-->>A: DecodingKey + Algorithm
        A->>A: full JWT validation (signature, exp, aud)
        A->>A: extract ferrox claims (allowed_models, rate_limit)
        A->>A: check per-tenant rate limit (RateLimitBackend)
    else static virtual key
        A->>A: lookup key in virtual_keys config
        A->>A: check per-key rate limit (RateLimitBackend)
    end

    A->>R: attach RequestContext, forward request

    R->>P: resolve model alias

    loop primary targets (LB strategy)
        P->>CB: is_available()?
        CB-->>P: true / false
    end

    P->>CB: select target
    CB->>Up: send request (with retry)

    alt success
        Up-->>C: response / SSE stream
    else all targets fail
        P->>P: try fallback chain
        P-->>C: 502 Bad Gateway
    end
```

---

## Concurrency model

The hot path (routing, circuit breaking, memory rate limiting) is entirely lock-free. The `RwLock` for the JWKS cache is taken only on TTL refresh — rare after warmup.

| Component | Primitive | Notes |
|---|---|---|
| Circuit breaker state | `AtomicU8` | CAS transitions between Closed/Open/HalfOpen |
| Circuit breaker probe guard | `AtomicBool` | CAS allows exactly one probe at a time |
| Failure/success counters | `AtomicU32` | Incremented with `fetch_add` |
| Token bucket (memory backend) | `AtomicU64` | CAS loop subtracts tokens |
| Round-robin counter | `AtomicUsize` | Monotonically incrementing, modulo target count |
| Weighted slot counter | `AtomicUsize` | Monotonically incrementing, modulo slot array length |
| JWKS key cache | `tokio::sync::RwLock` | Write held briefly on TTL refresh (background task) |
| MemoryBackend bucket map | `std::sync::RwLock` | Write held only when a new key is first seen |
| Redis backend | `deadpool-redis` async pool | One Lua round-trip per rated request |

The `AppState` struct is wrapped in `Arc` and cloned into each request handler. The rate limit backend (`Arc<dyn RateLimitBackend>`) is chosen at startup — memory or Redis — and is transparent to the rest of the gateway.

---

## Weighted load balancing

Weights are pre-expanded into a slot array at config load time. Example: weights `[70, 30]` are GCD-reduced to `[7, 3]`, then expanded to 10 slots: `[0,0,0,0,0,0,0,1,1,1]`. The hot path is a single atomic increment and a modulo lookup; no runtime division.

```mermaid
flowchart LR
    Weights["weights: [70, 30]"]
    GCD["GCD reduce: [7, 3]"]
    Slots["expand: [0,0,0,0,0,0,0,1,1,1]"]
    Counter["AtomicUsize counter"]
    Pick["counter % slots.len()"]

    Weights --> GCD --> Slots
    Counter --> Pick
    Slots --> Pick
    Pick --> Target["target index"]
```

---

## Streaming

SSE responses are passed through with a `stream!` adapter. Token usage from the final upstream chunk is recorded before the `[DONE]` sentinel is appended.

```mermaid
sequenceDiagram
    participant Up as Upstream
    participant F as Ferrox
    participant C as Client

    Up->>F: SSE chunk stream
    loop each chunk
        F->>F: parse chunk
        F->>F: record token usage (if usage present)
        F->>C: forward chunk as SSE event
    end
    F->>F: record latency + request metrics
    F->>C: data: [DONE]
```

---

## Circuit breaker state transitions

```mermaid
stateDiagram-v2
    [*] --> Closed : startup

    Closed --> Open : failure_count >= threshold\n(CAS Closed->Open)
    Open --> HalfOpen : recovery_timeout elapsed\n(CAS Open->HalfOpen)
    HalfOpen --> Closed : success_count >= threshold\n(CAS HalfOpen->Closed)
    HalfOpen --> Open : probe fails\n(CAS HalfOpen->Open)

    note right of HalfOpen
        Only one probe allowed at a time.
        AtomicBool CAS guards the probe slot.
    end note
```

---

## Error handling

All errors are represented by the `ProxyError` enum. It implements `axum::response::IntoResponse`, which maps each variant to the appropriate HTTP status and an OpenAI-compatible JSON body.

Non-retryable errors (401, 403, 404) short-circuit immediately. Retryable errors (5xx, 429, timeouts) go through the retry + fallback pipeline before producing a final error response.
