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
# Copy minimal config and set your keys in .env
cp config/config_minimal.yaml config/local.yaml

# Start the full stack
docker compose up
```

Services:

| Service | Port | URL |
|---|---|---|
| Ferrox | 8080 | `http://localhost:8080` |
| Grafana (LGTM) | 3000 | `http://localhost:3000` (admin/admin) |
| OTLP gRPC | 4317 | gRPC ingestion |
| OTLP HTTP | 4318 | HTTP ingestion |

The `grafana/otel-lgtm` image bundles Grafana, Loki, Tempo, Mimir, and the OTEL Collector into a single container — no separate services needed for local development.

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
