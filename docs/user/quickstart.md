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

Pull the image:

```bash
docker pull ghcr.io/shaharia-lab/ferrox:latest
```

Download the minimal config:

```bash
curl -Lo local.yaml https://raw.githubusercontent.com/shaharia-lab/ferrox/main/config/config_minimal.yaml
```

Run with your API keys:

```bash
docker run -p 8080:8080 \
  -e ANTHROPIC_API_KEY=sk-ant-... \
  -e OPENAI_API_KEY=sk-... \
  -v $(pwd)/local.yaml:/etc/ferrox/config.yaml \
  ghcr.io/shaharia-lab/ferrox:latest \
  ferrox --config /etc/ferrox/config.yaml
```

**Docker Compose** (full observability stack — requires cloning the repo):

```bash
git clone https://github.com/shaharia-lab/ferrox && cd ferrox
cp config/config_minimal.yaml config/local.yaml
# Set your keys in .env (cp .env.example .env)
docker compose up
```

| URL | Purpose |
|---|---|
| `http://localhost:8080` | Ferrox proxy |
| `http://localhost:3000` | Grafana dashboards (admin / admin) |

> **Note:** Docker Compose mounts the entire `config/` directory and reads `config/config.yaml` by default. The `local.yaml` copy is only needed if you want to customise settings without modifying the committed `config.yaml`.

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

**Binary / Homebrew / build from source** — copy the minimal config from the repo:

```bash
cp config/config_minimal.yaml config/local.yaml
```

**Docker (no repo clone)** — download the minimal config directly:

```bash
curl -Lo local.yaml https://raw.githubusercontent.com/shaharia-lab/ferrox/main/config/config_minimal.yaml
```

`config_minimal.yaml` includes Anthropic, OpenAI, and Gemini providers and four ready-to-use model aliases (`claude-sonnet`, `claude-haiku`, `gpt-4o`, `gemini-flash`). All timeouts, retries, and circuit breaker settings use production-ready defaults — no changes needed unless you want to customise.

## Set environment variables

**Binary / Homebrew / build from source** — create a `.env` file:

```bash
cp .env.example .env
```

Then set at least one provider key:

```bash
# .env
ANTHROPIC_API_KEY=sk-ant-...
PROXY_KEY=sk-local-dev       # your inbound virtual key
```

**Docker** — pass keys directly with `-e` flags (see the run command below). Keys for providers you don't use can be omitted — those providers will be skipped.

## Run

**Binary or Homebrew:**

```bash
set -a && . ./.env && set +a
LLM_PROXY_CONFIG=config/local.yaml ferrox
```

**Docker:**

```bash
docker run -p 8080:8080 \
  -e ANTHROPIC_API_KEY=sk-ant-... \
  -e PROXY_KEY=sk-local-dev \
  -v $(pwd)/local.yaml:/etc/ferrox/config.yaml \
  ghcr.io/shaharia-lab/ferrox:latest \
  ferrox --config /etc/ferrox/config.yaml
```

**Build from source (Makefile):**

```bash
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
