# Deployment

## Docker

### Build the image

```bash
docker build -t ferrox:local .
```

The Dockerfile uses a multi-stage build:

- **Builder**: `rust:1.94-slim-bookworm` with OpenSSL and protobuf-compiler
- **Runtime**: `debian:bookworm-slim` with only `ca-certificates` and `libssl3`

The runtime image runs as a non-root user.

### Run with Docker Compose

```bash
# Clone the repo if you haven't already
git clone https://github.com/shaharia-lab/ferrox && cd ferrox

# Copy minimal config — Compose reads config/config.yaml by default
cp config/config_minimal.yaml config/config.yaml   # or edit config/config.yaml directly

# Set your provider API keys
cp .env.example .env
# Edit .env and fill in at least one provider key

# Start the full stack
docker compose up
```

> **`config.yaml` vs `local.yaml`:** The `docker-compose.yml` mounts the entire `config/` directory and sets `LLM_PROXY_CONFIG=/app/config/config.yaml`. If you prefer to keep a clean committed baseline, copy `config_minimal.yaml` to `config/local.yaml` instead and update the `LLM_PROXY_CONFIG` value in your `.env` or `docker-compose.override.yml`.

Services:

| Service | Port | URL |
|---|---|---|
| Ferrox | 8080 | `http://localhost:8080` |
| ferrox-cp | 9090 | `http://localhost:9090` |
| PostgreSQL | — | internal only |
| Grafana (LGTM) | 3000 | `http://localhost:3000` (admin/admin) |
| OTLP gRPC | 4317 | gRPC ingestion |
| OTLP HTTP | 4318 | HTTP ingestion |

The `grafana/otel-lgtm` image bundles Grafana, Loki, Tempo, Mimir, and the OTEL Collector into a single container — no separate services needed for local development.

## Control plane (`ferrox-cp`)

`ferrox-cp` is an optional sidecar that issues short-lived JWTs to API clients.  Callers exchange a static `sk-cp-*` key for a JWT; the gateway then validates the JWT against the JWKS endpoint.

### Required environment variables

| Variable | Description |
|---|---|
| `DATABASE_URL` | PostgreSQL connection string (`postgres://ferrox:pass@host/ferrox_cp`) |
| `CP_ENCRYPTION_KEY` | 64 hex chars (32 bytes) — AES-256-GCM key for private keys at rest. Generate with `openssl rand -hex 32` |
| `CP_ADMIN_KEY` | Static bearer token for the admin REST API. Minimum 32 chars. |
| `CP_ISSUER` | JWT `iss` claim; must match `trusted_issuers[].issuer` in the gateway config (default: `https://ferrox-cp`) |
| `CP_PORT` | TCP port (default: `9090`) |

### Docker build

```bash
# Build from workspace root
docker build -f ferrox-cp/Dockerfile -t ferrox-cp:local .
```

### Start with Compose

```bash
# Set required control-plane variables in .env
CP_ENCRYPTION_KEY=$(openssl rand -hex 32)
CP_ADMIN_KEY=$(openssl rand -hex 20)

# Start postgres + ferrox-cp only
docker compose up postgres ferrox-cp

# Or start the full stack
docker compose up
```

### Enable JWT auth in the gateway

1. Start `ferrox-cp` and create a client:

```bash
API_KEY=$(curl -s -X POST http://localhost:9090/api/clients \
  -H "Authorization: Bearer $CP_ADMIN_KEY" \
  -H "Content-Type: application/json" \
  -d '{"name":"my-service","allowed_models":["claude-sonnet"],"rpm":60,"burst":10,"token_ttl_seconds":900}' \
  | jq -r .api_key)
```

2. Uncomment the `trusted_issuers` block in `ferrox/config/config_minimal.yaml` (or your local config).

3. Exchange the API key for a JWT and use it with the gateway:

```bash
JWT=$(curl -s -X POST http://localhost:9090/token \
  -H "Authorization: Bearer $API_KEY" | jq -r .access_token)

curl http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer $JWT" \
  -H "Content-Type: application/json" \
  -d '{"model":"claude-sonnet","messages":[{"role":"user","content":"Hello"}]}'
```

### Images

| Image | Registry |
|---|---|
| `ghcr.io/shaharia-lab/ferrox-cp:latest` | Published on every merge to `main` |
| `ghcr.io/shaharia-lab/ferrox-cp:<version>` | Published on GitHub release |

Place provider API keys in a `.env` file in the project root:

```bash
ANTHROPIC_API_KEY=sk-ant-...
OPENAI_API_KEY=sk-...
GEMINI_API_KEY=AIza...
```

### Health probes

| Probe | Path | Purpose |
|---|---|---|
| Liveness | `/healthz` | Restart if process is stuck |
| Readiness | `/readyz` | Remove from load balancer during drain |

The readiness probe returns `503` during graceful shutdown so the load balancer stops sending new traffic before the process exits.

### Resource sizing

Ferrox is CPU-bound only under very high concurrency. Memory usage is low and stable (no heap growth under load). Typical baseline:

```
CPU:    50–100m idle, spikes with concurrency
Memory: 32–64 MiB steady state
```

Adjust based on your actual traffic profile.
