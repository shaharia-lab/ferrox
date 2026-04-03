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
curl -Lo local.yaml https://raw.githubusercontent.com/shaharia-lab/ferrox/main/ferrox/config/config_minimal.yaml
```

Run with your API keys:

```bash
docker run -p 8080:8080 \
  -e ANTHROPIC_API_KEY=sk-ant-... \
  -e OPENAI_API_KEY=sk-... \
  -v $(pwd)/local.yaml:/etc/ferrox/config.yaml \
  ghcr.io/shaharia-lab/ferrox:latest \
  --config /etc/ferrox/config.yaml
```

**Docker Compose** (full observability stack — requires cloning the repo):

```bash
git clone https://github.com/shaharia-lab/ferrox && cd ferrox
make setup                             # creates .env with generated secrets
# Edit .env — set at least one provider key (ANTHROPIC_API_KEY etc.)
cp ferrox/config/config_minimal.yaml ferrox/config/config.yaml
docker compose up
```

| URL | Purpose |
|---|---|
| `http://localhost:8080` | Ferrox gateway |
| `http://localhost:9090` | Control plane admin UI |
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
curl -Lo local.yaml https://raw.githubusercontent.com/shaharia-lab/ferrox/main/ferrox/config/config_minimal.yaml
```

`config_minimal.yaml` includes Anthropic, OpenAI, and Gemini providers and four ready-to-use model aliases (`claude-sonnet`, `claude-haiku`, `gpt-4o`, `gemini-flash`). All timeouts, retries, and circuit breaker settings use production-ready defaults — no changes needed unless you want to customise.

## Set environment variables

**Binary / Homebrew / build from source** — run the setup target, which copies
`.env.example` and auto-generates the required control-plane secrets:

```bash
make setup
```

Then open `.env` and set at least one provider key:

```bash
# .env
ANTHROPIC_API_KEY=sk-ant-...
PROXY_KEY=sk-local-dev       # your inbound virtual key
```

`CP_ENCRYPTION_KEY` and `CP_ADMIN_KEY` are filled in automatically by `make setup`.
If you prefer to set them manually: `CP_ENCRYPTION_KEY` must be exactly 64 hex chars
(`openssl rand -hex 32`), and `CP_ADMIN_KEY` must be at least 32 chars.

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
  --config /etc/ferrox/config.yaml
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

## Optional: Control plane (`ferrox-cp`)

`ferrox-cp` lets you issue short-lived JWTs to client services instead of sharing long-lived static keys.  This step is entirely optional — the gateway works fine with virtual keys alone.

### Start the control plane

```bash
# Generate secrets (add to .env)
echo "CP_ENCRYPTION_KEY=$(openssl rand -hex 32)" >> .env
echo "CP_ADMIN_KEY=$(openssl rand -hex 20)"      >> .env

# Start postgres + control plane alongside the gateway
docker compose up postgres ferrox-cp ferrox
```

### Access the admin UI

Open `http://localhost:9090` in your browser and sign in with your `CP_ADMIN_KEY`.  The UI
lets you create and revoke clients, view token usage, rotate signing keys, and browse the audit
log — all without touching the REST API directly.

### Create a client via the REST API (or use the UI above)

```bash
# Load the admin key from .env
source .env

# Create a client (api_key shown once)
API_KEY=$(curl -s -X POST http://localhost:9090/api/clients \
  -H "Authorization: Bearer $CP_ADMIN_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "my-service",
    "allowed_models": ["claude-sonnet"],
    "rpm": 60,
    "burst": 10,
    "token_ttl_seconds": 900
  }' | jq -r .api_key)

# Exchange for a JWT (valid for 15 minutes)
JWT=$(curl -s -X POST http://localhost:9090/token \
  -H "Authorization: Bearer $API_KEY" | jq -r .access_token)
```

### Enable JWT validation in the gateway

Uncomment the `trusted_issuers` block in your gateway config:

```yaml
trusted_issuers:
  - issuer: "${CP_ISSUER:-http://localhost:9090}"
    jwks_uri: "${CP_JWKS_URI:-http://localhost:9090/.well-known/jwks.json}"
    audience: "ferrox"
```

> **Local vs Docker Compose:** when running from source, `CP_JWKS_URI` defaults to
> `http://localhost:9090/...` which is correct.  Docker Compose sets
> `CP_JWKS_URI=http://ferrox-cp:9090/.well-known/jwks.json` automatically via the
> internal service hostname.

Then use the JWT to call the gateway:

```bash
curl http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer $JWT" \
  -H "Content-Type: application/json" \
  -d '{"model":"claude-sonnet","messages":[{"role":"user","content":"Hello"}]}'
```

## Next steps

- [Configuration reference](configuration.md) — all config options
- [Providers](providers.md) — add Bedrock, GLM, or additional API keys
- [Routing](routing.md) — set up failover and weighted routing
- [Virtual keys](virtual-keys.md) — issue scoped keys for each service
