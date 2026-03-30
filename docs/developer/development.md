# Development

## Prerequisites

- Rust 1.74+ (`rustup update stable`)
- `protobuf-compiler` (for opentelemetry-otlp/tonic build)
  - Ubuntu/Debian: `sudo apt install protobuf-compiler`
  - macOS: `brew install protobuf`
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
make help           # list all targets
```

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

```bash
cargo test --workspace

# Specific module
cargo test -p ferrox config::tests

# Show stdout for all tests
cargo test --workspace -- --nocapture
```

## Run locally

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

Or with the full observability stack (Grafana + Loki + Tempo + Prometheus + OTEL Collector bundled in one container):

```bash
make docker-up
# Grafana UI: http://localhost:3000  (admin / admin)
# OTLP gRPC:  localhost:4317
# OTLP HTTP:  localhost:4318
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
    auth.rs           auth middleware
    router.rs         model alias resolution
    error.rs          error types and HTTP responses
    types.rs          OpenAI wire types
    retry.rs          retry logic with backoff
    metrics.rs        startup metrics shim

    providers/        one file per provider
    lb/               load balancing and circuit breakers
    ratelimit/        token bucket rate limiter
    telemetry/        logging, metrics, tracing
    handlers/         HTTP request handlers

  config/
    config.yaml           example configuration
    config_minimal.yaml   quickstart template
  config.schema.json      JSON Schema for config validation

ferrox-cp/          control plane binary crate (Phase 3)
  Cargo.toml
  src/
    main.rs

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
3. `cargo test --workspace`
4. `cargo build --release --workspace`
