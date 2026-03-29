# Quick Start

This guide gets Ferrox running locally in under 5 minutes.

## Prerequisites

- Rust 1.74+ (`rustup update stable`)
- At least one LLM provider API key (Anthropic, OpenAI, or Gemini)

## 1. Clone and build

```bash
git clone https://github.com/shaharia-lab/ferrox
cd ferrox
cargo build --release
```

## 2. Configure

Copy the minimal config (pre-configured with sensible defaults):

```bash
cp config/config_minimal.yaml config/local.yaml
```

`config_minimal.yaml` includes Anthropic, OpenAI, and Gemini providers and four ready-to-use model aliases. All timeouts, retries, and circuit breaker settings use production-ready defaults — no changes needed unless you want to customise.

## 3. Set environment variables

Create a `.env` file:

```bash
cp .env.example .env
```

Then set at least one provider key:

```bash
# .env
ANTHROPIC_API_KEY=sk-ant-...
PROXY_KEY=sk-local-dev       # your inbound virtual key
```

Keys for providers you don't use can be left blank — those providers will be skipped.

## 4. Run

```bash
make run
# or directly:
# set -a && . ./.env && set +a
# LLM_PROXY_CONFIG=config/local.yaml ./target/release/ferrox
```

## 5. Send a request

```bash
curl http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer sk-local-dev" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "claude-sonnet",
    "messages": [{"role": "user", "content": "Say hello in one sentence."}]
  }'
```

Available model aliases out of the box: `claude-sonnet`, `claude-haiku`, `gpt-4o`, `gemini-flash`.

## 6. Verify health

```bash
curl http://localhost:8080/healthz   # -> {"status":"ok"}
curl http://localhost:8080/readyz    # -> "ready" when startup is complete
curl http://localhost:8080/metrics   # -> Prometheus text format
```

## Using Docker Compose

Starts Ferrox and the full LGTM observability stack (Grafana, Loki, Tempo, Mimir + OTEL Collector) in a single command:

```bash
cp config/config_minimal.yaml config/local.yaml
# Edit config/local.yaml or set keys in .env
docker compose up
```

| URL | Purpose |
|---|---|
| `http://localhost:8080` | Ferrox proxy |
| `http://localhost:3000` | Grafana dashboards (admin / admin) |

## Next steps

- [Configuration reference](configuration.md) — all config options
- [Providers](providers.md) — add Bedrock, GLM, or additional API keys
- [Routing](routing.md) — set up failover and weighted routing
- [Virtual keys](virtual-keys.md) — issue scoped keys for each service
