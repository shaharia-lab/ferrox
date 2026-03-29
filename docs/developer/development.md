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
make build          # debug build
make build-release  # release build
make test           # run all tests
make fmt            # format code
make lint           # clippy
make check          # fmt-check + lint + test (CI equivalent)
make run            # run dev server (loads .env, uses config/local.yaml)
make docker-up      # start full stack with Docker Compose
make help           # list all targets
```

## Build

```bash
cargo build
cargo build --release
```

## Run tests

```bash
cargo test

# Specific module
cargo test config::tests

# Show stdout for all tests
cargo test -- --nocapture
```

## Run locally

```bash
# 1. Copy the example env file and fill in your API keys
cp .env.example .env

# 2. Create a local config from the minimal template (Makefile does this automatically on first run)
cp config/config_minimal.yaml config/local.yaml

# 3. Run (Makefile loads .env automatically)
make run

# Or manually:
set -a && . ./.env && set +a
LLM_PROXY_CONFIG=config/local.yaml cargo run
```

Or with the full observability stack (Grafana + Loki + Tempo + Prometheus + OTEL Collector bundled in one container):

```bash
make docker-up
# Grafana UI: http://localhost:3000  (admin / admin)
# OTLP gRPC:  localhost:4317
# OTLP HTTP:  localhost:4318
```

## Project structure

```
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
  config.yaml       example configuration

docs/
  user/             user-facing guides
  developer/        this directory
```

## Adding a new provider

1. Create `src/providers/yourprovider.rs`
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

3. Add a new variant to `ProviderType` in `config.rs`
4. Register the adapter in `providers/mod.rs` inside `build_registry()`
5. Add a test covering at least the request transformation

## Adding a new routing strategy

1. Add a variant to `LbStrategy` in `lb/strategy.rs`
2. Implement the `select(&[bool]) -> Option<usize>` match arm
3. Add a corresponding variant to `RoutingStrategy` in `config.rs`
4. Wire it in `lb/mod.rs` inside `RoutePool::from_config()`
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

1. `cargo fmt --check`
2. `cargo clippy -- -D warnings`
3. `cargo test`
4. `cargo build --release`
