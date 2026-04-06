# Development

## Prerequisites

- Rust 1.74+ (`rustup update stable`)
- `protobuf-compiler` (for opentelemetry-otlp/tonic build)
  - Ubuntu/Debian: `sudo apt install protobuf-compiler`
  - macOS: `brew install protobuf`
- Node.js 18+ and npm (for the ferrox-cp admin UI)
  - Ubuntu/Debian: `sudo apt install nodejs npm` or use [nvm](https://github.com/nvm-sh/nvm)
  - macOS: `brew install node`
- Docker (optional, for integration stack)
- `pre-commit` (strongly recommended)
  - `pip install pre-commit` or `brew install pre-commit`

## Pre-commit hooks

Install hooks once after cloning:

```bash
pre-commit install
```

This runs `cargo fmt` and `cargo clippy -- -D warnings` automatically on every commit, catching issues before they reach CI. All contributors are expected to have the hooks installed.

## Common tasks (Makefile)

```bash
make build          # debug build (all workspace members)
make build-release  # release build (all workspace members)
make test           # run all tests (all workspace members)
make fmt            # format code
make lint           # clippy
make check          # fmt-check + lint + test (CI equivalent)
make run            # run dev server (loads .env, uses ferrox/config/local.yaml)
make docker-up      # start full stack with Docker Compose
make ui-install     # npm ci for the ferrox-cp admin UI
make ui-build       # build the admin UI (outputs to ferrox-cp/ui/dist/)
make ui-dev         # start Vite dev server proxying to localhost:9090
make help           # list all targets
```

> **Admin UI and `cargo build`:** `ferrox-cp/ui/dist/index.html` is a committed placeholder
> so `cargo build -p ferrox-cp` always succeeds even without a prior UI build.  Run
> `make ui-build` once (or whenever you change UI source) to embed the real UI.

## Build

```bash
# Build all workspace members
cargo build --workspace
cargo build --release --workspace

# Build a specific package
cargo build -p ferrox
cargo build -p ferrox-cp
```

## Run tests

### Gateway tests (no database required)

```bash
cargo test -p ferrox

# Specific module
cargo test -p ferrox config::tests

# Show stdout
cargo test -p ferrox -- --nocapture
```

### Control-plane tests (requires PostgreSQL)

The `ferrox-cp` integration tests use `#[sqlx::test]` which spins up isolated temporary databases. You need a running Postgres instance:

```bash
# Start a local Postgres container (first time only)
docker run --name ferrox-test-pg \
  -e POSTGRES_PASSWORD=testpass \
  -p 5433:5432 \
  -d postgres:16-alpine

# Run ferrox-cp tests
DATABASE_URL="postgres://postgres:testpass@localhost:5433/postgres" \
  cargo test -p ferrox-cp -- --test-threads=1

# Or export the variable for the session
export DATABASE_URL="postgres://postgres:testpass@localhost:5433/postgres"
cargo test -p ferrox-cp -- --test-threads=1
```

> **Why `--test-threads=1`?** `#[sqlx::test]` creates a temporary database per test. The `config::tests` module mutates `DATABASE_URL` via environment variables; serialising all tests prevents races between those env-var writes and the DB integration tests.

### All workspace tests

```bash
# Set DATABASE_URL first (see above), then:
cargo test --workspace -- --test-threads=1
```

## Run locally

### Gateway only (quickest start)

```bash
# 1. Copy the example env file and fill in your API keys
cp .env.example .env

# 2. Create a local config from the minimal template (Makefile does this automatically on first run)
cp ferrox/config/config_minimal.yaml ferrox/config/local.yaml

# 3. Run (Makefile loads .env automatically)
make run

# Or manually:
set -a && . ./.env && set +a
LLM_PROXY_CONFIG=ferrox/config/local.yaml cargo run -p ferrox
```

The gateway runs standalone — it only needs provider API keys. Control plane, usage tracking, and budget enforcement are all optional.

### Control plane (ferrox-cp)

The control plane requires PostgreSQL and a few environment variables. All are defined in `.env.example` — fill them in your `.env` file.

```bash
# 1. Start a local Postgres instance
docker run --name ferrox-cp-pg \
  -e POSTGRES_DB=ferrox_cp \
  -e POSTGRES_USER=ferrox \
  -e POSTGRES_PASSWORD=ferrox \
  -p 5432:5432 \
  -d postgres:16-alpine

# 2. Generate required secrets and add to your .env file
#    CP_ENCRYPTION_KEY — 64 hex chars, encrypts private keys at rest
openssl rand -hex 32
#    CP_ADMIN_KEY — admin API bearer token, min 32 chars
openssl rand -hex 20

# 3. Run the control plane (loads .env automatically)
set -a && . ./.env && set +a
cargo run -p ferrox-cp
```

Required environment variables (see `.env.example` for the full list):

| Variable | Description |
|---|---|
| `DATABASE_URL` | PostgreSQL connection string |
| `CP_ENCRYPTION_KEY` | 64 hex chars — AES-256-GCM key for private keys at rest |
| `CP_ADMIN_KEY` | Bearer token for the admin REST API and UI |
| `CP_ISSUER` | JWT `iss` claim value — must match gateway `trusted_issuers` |

The control plane runs on port 9090 by default (`CP_PORT`). On startup it:
- Runs database migrations automatically
- Generates an RSA-2048 signing key (if none exist)
- Starts background tasks (key retirement, budget enforcement)

**Admin UI:** Open http://localhost:9090 and enter the `CP_ADMIN_KEY` to log in.

### Gateway + control plane together

To connect the gateway to the control plane for JWT-based auth and usage tracking:

```bash
# In your .env or local.yaml, uncomment the trusted_issuers section:
trusted_issuers:
  - issuer: "http://localhost:9090"
    jwks_uri: "http://localhost:9090/.well-known/jwks.json"
    audience: "ferrox"

# Enable usage tracking (optional — points to the same database):
usage_database_url: "postgres://ferrox:ferrox@localhost:5432/ferrox_cp"
```

Then start both services (in separate terminals). Both load from `.env`:

```bash
# Terminal 1: control plane
set -a && . ./.env && set +a
cargo run -p ferrox-cp

# Terminal 2: gateway
make run
```

Create a client and obtain a JWT:

```bash
# Create an API client
curl -s -X POST http://localhost:9090/api/clients \
  -H "Authorization: Bearer $CP_ADMIN_KEY" \
  -H "Content-Type: application/json" \
  -d '{"name":"my-app","allowed_models":["*"],"rpm":100,"burst":10,"token_ttl_seconds":900}' \
  | jq .

# Exchange the API key for a JWT (use the api_key from the response above)
curl -s -X POST http://localhost:9090/token \
  -H "Authorization: Bearer sk-cp-<key-from-above>" \
  | jq .

# Use the JWT with the gateway
curl http://localhost:8080/v1/models \
  -H "Authorization: Bearer <jwt-from-above>"
```

### Full stack (Docker Compose)

Runs everything together — gateway, control plane, PostgreSQL, and the observability stack:

```bash
make docker-up
# Gateway:    http://localhost:8080
# Control plane: http://localhost:9090  (Admin UI)
# Grafana:    http://localhost:3000  (admin / admin)
# OTLP gRPC: localhost:4317
# OTLP HTTP: localhost:4318
```

## Project structure

This is a Cargo workspace. The root `Cargo.toml` is the workspace manifest.

```
Cargo.toml          workspace manifest (members: ferrox, ferrox-cp)
Cargo.lock          shared workspace lock file

ferrox/             gateway binary crate
  Cargo.toml
  build.rs          version embedding (git SHA at compile time)
  src/
    main.rs           entry point
    server.rs         HTTP router construction
    config.rs         config loading and validation
    state.rs          shared application state
    auth.rs           auth middleware + budget reservation
    router.rs         model alias resolution
    error.rs          error types and HTTP responses
    types.rs          OpenAI wire types
    retry.rs          retry logic with backoff
    metrics.rs        startup metrics shim
    usage_writer.rs   async batched writer (mpsc → PostgreSQL usage_log)
    budget_enforcer.rs  Redis-backed budget check + reconciliation (Lua scripts)

    providers/        one file per provider
    lb/               load balancing and circuit breakers
    ratelimit/        token bucket rate limiter
    telemetry/        logging, metrics, tracing
    handlers/         HTTP request handlers

  config/
    config.yaml           example configuration
    config_minimal.yaml   quickstart template
  config.schema.json      JSON Schema for config validation

ferrox-cp/          control plane binary crate
  Cargo.toml
  Dockerfile        multi-stage: node:22-slim UI build → rust builder → debian slim
  migrations/
    20240001000000_initial_schema.sql
    20240002000000_usage_log.sql
    20240003000000_client_budgets.sql
  src/
    main.rs           entry point, MIGRATOR static, startup key seeding, background tasks
    budget.rs         periodic budget checker (revokes over-budget clients)
    config.rs         CpConfig loaded from env vars
    error.rs          CpError (top-level error enum)
    state.rs          CpState (db pool + config)
    ui.rs             SPA handler: embeds ui/dist via include_dir!, serves static
                      files by MIME type, falls back to index.html for SPA routes
    crypto/
      mod.rs
      keys.rs         RSA-2048 keypair generation (PKCS#1 DER output)
      encrypt.rs      AES-256-GCM encrypt/decrypt for private keys at rest
      jwks.rs         DER public key → JWK (RFC 7517)
      jwt.rs          JwtSigner — sign JWTs (includes budget claims)
    handlers/
      mod.rs
      health.rs       GET /healthz
      jwks.rs         GET /.well-known/jwks.json
      token.rs        POST /token — API key → JWT exchange
      admin/
        mod.rs
        clients.rs    CRUD + revoke + usage + budget + reactivate
        signing_keys.rs  list + rotate signing keys
        audit.rs      filterable audit log
    middleware/
      admin_auth.rs   Bearer token check (subtle::ConstantTimeEq)
    db/
      mod.rs
      models.rs       Client, SigningKey, AuditEntry, AuditEvent, UsageRecord, UsageSummary
      error.rs        RepoError
      client_repo.rs  CRUD + budget + reactivate + find_over_budget
      signing_key_repo.rs
      audit_repo.rs
      usage_repo.rs   batch insert + summarize + paginated list
  ui/               React + TypeScript admin SPA
    package.json
    vite.config.ts  proxies /api → localhost:9090 in dev mode
    tailwind.config.ts
    index.html      entry point (production HTML is generated by Vite build)
    dist/
      index.html    placeholder committed to repo; real assets built by CI/Docker
    src/
      main.tsx      QueryClient + BrowserRouter setup
      App.tsx       route definitions, auth gate
      api.ts        typed fetch wrapper, admin key storage
      index.css     Tailwind base styles
      pages/
        Login.tsx         admin key entry (validates before persisting)
        Dashboard.tsx     overview stats + recent audit events
        Clients.tsx       client table, create modal, revoke confirm
        ClientDetail.tsx  usage chart (Recharts), per-client audit log
        SigningKeys.tsx    key table with active/retiring/retired badges
        AuditLog.tsx      filterable audit table with expandable metadata
      components/
        Layout.tsx        sidebar nav + sign-out
        ui/               minimal Tailwind component library
          Button.tsx
          Badge.tsx
          Card.tsx
          Dialog.tsx
          Input.tsx
          Label.tsx

docs/
  user/             user-facing guides
  developer/        this directory
```

## Adding a new provider

1. Create `ferrox/src/providers/yourprovider.rs`
2. Implement the `ProviderAdapter` trait:

```rust
#[async_trait]
impl ProviderAdapter for YourAdapter {
    fn name(&self) -> &str { "your-provider" }

    async fn chat(
        &self,
        req: &ChatCompletionRequest,
        model_id: &str,
    ) -> Result<ChatCompletionResponse, ProxyError> {
        // transform request, call API, transform response
    }

    async fn chat_stream(
        &self,
        req: &ChatCompletionRequest,
        model_id: &str,
    ) -> Result<ProviderStream, ProxyError> {
        // transform request, call streaming API, return SSE stream
    }
}
```

3. Add a new variant to `ProviderType` in `ferrox/src/config.rs`
4. Register the adapter in `ferrox/src/providers/mod.rs` inside `build_registry()`
5. Add a test covering at least the request transformation

## Adding a new routing strategy

1. Add a variant to `LbStrategy` in `ferrox/src/lb/strategy.rs`
2. Implement the `select(&[bool]) -> Option<usize>` match arm
3. Add a corresponding variant to `RoutingStrategy` in `ferrox/src/config.rs`
4. Wire it in `ferrox/src/lb/mod.rs` inside `RoutePool::from_config()`
5. Add unit tests

## Testing guidelines

- Unit tests live in `#[cfg(test)] mod tests` at the bottom of each source file.
- Use `AtomicU32` counters to verify call counts in retry tests.
- Use `initial_backoff_ms: 0` in retry configs to prevent actual sleeps in tests.
- Prometheus metrics use `once_cell::sync::Lazy` to avoid duplicate registration panics across tests.
- Do not mock the provider HTTP layer in unit tests; test logic and transformation functions directly.

## Code style

- `cargo fmt` before committing
- `cargo clippy -- -D warnings` must pass
- No `unwrap()` in non-test code except inside `Lazy::new(|| ...)` metric registrations (panics there indicate a programming error, not a runtime error)
- Prefer `Arc<T>` over `Rc<T>` everywhere (we are multi-threaded)
- Log with structured fields using `tracing::info!(field = %value, "message")`

## CI checks

The CI pipeline runs:

1. `cargo fmt --all --check`
2. `cargo clippy --workspace -- -D warnings`
3. `cargo test --workspace -- --test-threads=1` (with a Postgres 16 service container)
4. `cargo build --release --workspace`
5. `cd ferrox-cp/ui && npm ci && npm run build` (TypeScript type-check + Vite build)
6. Config schema validation (`check-jsonschema`)
7. `cargo audit` (security advisories)
8. Docker image build for `ferrox-cp` (push to GHCR on merge to `main`)
