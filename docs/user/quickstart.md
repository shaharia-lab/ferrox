# Quick Start

This guide gets Ferrox running locally in under 5 minutes.

## Installation

Choose the method that suits you best.

### Option 1: Pre-built binary (fastest)

Download the latest binary for your platform from [GitHub Releases](https://github.com/shaharia-lab/ferrox/releases/latest):

```bash
# macOS (Apple Silicon)
curl -L https://github.com/shaharia-lab/ferrox/releases/latest/download/ferrox-aarch64-apple-darwin.tar.gz | tar xz
sudo mv ferrox /usr/local/bin/

# macOS (Intel)
curl -L https://github.com/shaharia-lab/ferrox/releases/latest/download/ferrox-x86_64-apple-darwin.tar.gz | tar xz
sudo mv ferrox /usr/local/bin/

# Linux (x86_64)
curl -L https://github.com/shaharia-lab/ferrox/releases/latest/download/ferrox-x86_64-unknown-linux-gnu.tar.gz | tar xz
sudo mv ferrox /usr/local/bin/

# Linux (ARM64)
curl -L https://github.com/shaharia-lab/ferrox/releases/latest/download/ferrox-aarch64-unknown-linux-gnu.tar.gz | tar xz
sudo mv ferrox /usr/local/bin/
```

Verify the install:

```bash
ferrox --version
```

### Option 2: Homebrew (macOS and Linux)

```bash
brew install shaharia-lab/tap/ferrox
```

To install a specific version:

```bash
brew install shaharia-lab/tap/ferrox@1.0.0
```

### Option 3: Docker

```bash
docker pull ghcr.io/shaharia-lab/ferrox:latest
```

Run with a config file:

```bash
docker run -p 8080:8080 \
  -e ANTHROPIC_API_KEY=sk-ant-... \
  -v $(pwd)/config/local.yaml:/etc/ferrox/config.yaml \
  ghcr.io/shaharia-lab/ferrox:latest \
  ferrox --config /etc/ferrox/config.yaml
```

Or use Docker Compose for the full observability stack:

```bash
cp config/config_minimal.yaml config/local.yaml
# Edit config/local.yaml or set keys in .env
docker compose up
```

| URL | Purpose |
|---|---|
| `http://localhost:8080` | Ferrox proxy |
| `http://localhost:3000` | Grafana dashboards (admin / admin) |

### Option 4: Build from source

```bash
# Prerequisites: Rust 1.74+, protobuf-compiler
# Ubuntu/Debian: sudo apt install protobuf-compiler
# macOS: brew install protobuf

git clone https://github.com/shaharia-lab/ferrox
cd ferrox
cargo build --release
# Binary at: ./target/release/ferrox
```

## Configure

Copy the minimal config (pre-configured with sensible defaults):

```bash
cp config/config_minimal.yaml config/local.yaml
```

`config_minimal.yaml` includes Anthropic, OpenAI, and Gemini providers and four ready-to-use model aliases. All timeouts, retries, and circuit breaker settings use production-ready defaults — no changes needed unless you want to customise.

## Set environment variables

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

## Run

```bash
# If installed as a binary or via Homebrew:
set -a && . ./.env && set +a
LLM_PROXY_CONFIG=config/local.yaml ferrox

# If built from source (using Makefile):
make run
```

## Send a request

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

## Verify health

```bash
curl http://localhost:8080/healthz   # -> {"status":"ok"}
curl http://localhost:8080/readyz    # -> "ready" when startup is complete
curl http://localhost:8080/metrics   # -> Prometheus text format
```

## Next steps

- [Configuration reference](configuration.md) — all config options
- [Providers](providers.md) — add Bedrock, GLM, or additional API keys
- [Routing](routing.md) — set up failover and weighted routing
- [Virtual keys](virtual-keys.md) — issue scoped keys for each service
