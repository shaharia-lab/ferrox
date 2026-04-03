# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**Ferrox** is a stateless, horizontally-scalable LLM API gateway written in Rust that exposes an OpenAI-compatible API and routes requests to multiple LLM providers (Anthropic, OpenAI, Gemini, AWS Bedrock, GLM). It consists of two binaries in a Cargo workspace:

- `ferrox` — the gateway binary (request routing, rate limiting, circuit breaking)
- `ferrox-cp` — the control plane binary (JWT key management, API client CRUD, embedded React admin UI)

## Essential Commands

```bash
# First-time setup
make setup              # Generate .env with auto-generated secrets (idempotent)

# Development
make run                # Run gateway with ferrox/config/local.yaml (loads .env)
make build              # Debug build (cargo build --workspace)
make build-release      # Release build

# Quality checks (run before committing)
make fmt                # Format code
make lint               # cargo clippy --workspace -- -D warnings
make check              # fmt-check + lint + test (CI equivalent)

# Testing
make test                     # cargo test --workspace (requires PostgreSQL for ferrox-cp)
cargo test -p ferrox          # Gateway tests only (no DB required)
cargo test -p ferrox-cp -- --test-threads=1  # CP integration tests (requires DATABASE_URL)

# UI (ferrox-cp/ui/)
make ui-install         # npm ci
make ui-build           # npm run build → ferrox-cp/ui/dist/
make ui-dev             # Vite dev server (proxies /api to :9090)

# Docker
make docker-up          # Full stack: gateway + CP + PostgreSQL + LGTM (foreground)
make docker-up-detached # Same but detached
make docker-down        # Stop stack
```

**Running a single test:**
```bash
cargo test -p ferrox config::tests              # By module path
cargo test -p ferrox test_name -- --nocapture   # By test name, show stdout
```

**ferrox-cp integration tests** use `#[sqlx::test]` which automatically creates isolated temporary databases. Set `DATABASE_URL` to a PostgreSQL superuser connection (not the app DB):
```bash
DATABASE_URL="postgres://postgres:testpass@localhost:5433/postgres" \
  cargo test -p ferrox-cp -- --test-threads=1
```

## Architecture

### Request Flow

```
Client → [auth middleware] → [rate limiter] → ModelRouter
  → RoutePool → [circuit breaker] → [retry] → Provider adapter → LLM API
```

1. **Auth** (`ferrox/src/auth.rs`): Bearer token validates against virtual keys or JWT (JWKS-backed)
2. **JWKS cache** (`ferrox/src/jwks.rs`): TTL refresh with stale fallback; `jwks_uri` points to ferrox-cp
3. **ModelRouter** (`ferrox/src/router.rs`): Resolves model alias → `RoutePool`
4. **RoutePool** (`ferrox/src/lb/`): Selects target using configured strategy
5. **Circuit Breaker** (`ferrox/src/lb/circuit_breaker.rs`): Per-provider+model, lock-free via `AtomicU8`
6. **Provider Adapters** (`ferrox/src/providers/`): Translate OpenAI format to/from each provider's API

### Key Design Patterns

**Lock-free hot path**: Circuit breaker state (`AtomicU8`), token buckets (`AtomicU64` CAS loop), round-robin counter (`AtomicUsize`), and weighted slot selection all avoid mutexes on the request path.

**Rate limiting**: Memory backend (default, per-instance token buckets) or Redis backend (Lua script, distributed). Configured per virtual key.

**Load balancing strategies**: `RoundRobin`, `Weighted` (pre-expanded slot array + atomic modulo), `Failover` (primary + fallback chain), `Random`.

**Circuit breaker states**: `Closed → Open` (failure threshold) `→ HalfOpen` (timeout) `→ Closed` (success). Single probe at a time via `AtomicBool` CAS.

**Streaming**: SSE pass-through; token usage metrics are recorded before the `[DONE]` sentinel.

**Control plane crypto**: RSA-2048 keypairs generated at startup (idempotent). Private keys encrypted with AES-256-GCM before storage in PostgreSQL. Decrypted on-demand for JWT signing.

**Database pattern** (ferrox-cp): SQLx runtime queries (no compile-time macros). Repository pattern per table. Migrations embedded via `sqlx::migrate!` and applied at startup.

### Service Ports (Docker Compose)

| Service | Port | Notes |
|---------|------|-------|
| Ferrox gateway | 8080 | OpenAI-compatible API |
| Control plane | 9090 | Admin UI + JWKS + token endpoint |
| Grafana | 3000 | admin/admin |
| OTLP gRPC | 4317 | |
| OTLP HTTP | 4318 | |

## Configuration

**Gateway config** is YAML, validated against `ferrox/config.schema.json`. Key sections: `server`, `telemetry`, `defaults` (retry/circuit breaker/timeout defaults), `providers`, `models` (aliases → routing strategies), `rate_limiting`, `trusted_issuers`, `virtual_keys`.

**Control plane** is configured entirely via environment variables (see `.env.example`): `DATABASE_URL`, `CP_ENCRYPTION_KEY` (64 hex chars), `CP_ADMIN_KEY`, `CP_ISSUER`, `CP_PORT`.

## Available Sub-Agents

Project-scoped agents live in `.claude/agents/`. Invoke with `@<name>` in any Claude Code session.

| Agent | File | Responsibility |
|-------|------|----------------|
| `architecture-guardian` | `.claude/agents/architecture-guardian.md` | Reviews proposed designs and audits existing architecture for inconsistencies, non-optimal patterns, unnecessary complexity, and performance issues. Asks clarifying questions before acting. |
| `security-reviewer` | `.claude/agents/security-reviewer.md` | Reviews PRs and audits the full codebase for security vulnerabilities, attack vectors, CVEs, cryptographic issues, auth flaws, and runtime risks. |

## Documentation Index

| Topic | Path |
|-------|------|
| Quickstart (5-min setup) | `docs/user/quickstart.md` |
| Full configuration reference | `docs/user/configuration.md` |
| Provider setup (Anthropic, OpenAI, Gemini, Bedrock) | `docs/user/providers.md` |
| Routing strategies & failover | `docs/user/routing.md` |
| Virtual keys & rate limiting | `docs/user/virtual-keys.md` |
| API endpoint reference | `docs/user/api-reference.md` |
| Metrics, tracing, logging | `docs/user/observability.md` |
| System design & request flow | `docs/developer/architecture.md` |
| Build, test, develop guide | `docs/developer/development.md` |
| Docker, control plane, admin UI deployment | `docs/developer/deployment.md` |
| Contribution guidelines | `CONTRIBUTING.md` |
| Security policy | `SECURITY.md` |
| Full config example (all features) | `ferrox/config/config.yaml` |
| Minimal config template | `ferrox/config/config_minimal.yaml` |
| Config JSON Schema | `ferrox/config.schema.json` |
| Environment variable template | `.env.example` |
| PostgreSQL schema migration | `ferrox-cp/migrations/20240001000000_initial_schema.sql` |
