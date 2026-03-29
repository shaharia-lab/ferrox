.PHONY: build build-release test fmt lint check run run-release clean \
        docker-build docker-up docker-down docker-logs help

# ── Build ──────────────────────────────────────────────────────────────────────

build:
	cargo build

build-release:
	cargo build --release

# ── Quality ────────────────────────────────────────────────────────────────────

test:
	cargo test

fmt:
	cargo fmt

fmt-check:
	cargo fmt --check

lint:
	cargo clippy -- -D warnings

## Run fmt-check + lint + test (same as CI)
check: fmt-check lint test

# ── Run locally ────────────────────────────────────────────────────────────────

## Copy minimal config template if local config does not exist yet
config/local.yaml:
	cp config/config_minimal.yaml config/local.yaml
	@echo "Created config/local.yaml from config_minimal.yaml — set your API keys in .env before running."

## Run dev server (requires config/local.yaml and .env)
run: config/local.yaml
	@if [ -f .env ]; then \
		set -a && . ./.env && set +a && LLM_PROXY_CONFIG=config/local.yaml cargo run; \
	else \
		LLM_PROXY_CONFIG=config/local.yaml cargo run; \
	fi

## Run release binary
run-release: config/local.yaml build-release
	@if [ -f .env ]; then \
		set -a && . ./.env && set +a && LLM_PROXY_CONFIG=config/local.yaml ./target/release/ferrox; \
	else \
		LLM_PROXY_CONFIG=config/local.yaml ./target/release/ferrox; \
	fi

# ── Health checks (requires a running instance) ───────────────────────────────

health:
	curl -sf http://localhost:8080/healthz && echo
	curl -sf http://localhost:8080/readyz  && echo

metrics:
	curl -s http://localhost:8080/metrics

# ── Docker ─────────────────────────────────────────────────────────────────────

docker-build:
	docker build -t ferrox:local .

## Start full stack: Ferrox + Prometheus + Grafana + Jaeger + OTEL Collector
docker-up:
	docker compose up --build

docker-up-detached:
	docker compose up --build -d

docker-down:
	docker compose down

docker-logs:
	docker compose logs -f ferrox

# ── Cleanup ────────────────────────────────────────────────────────────────────

clean:
	cargo clean

# ── Help ───────────────────────────────────────────────────────────────────────

help:
	@echo ""
	@echo "Usage: make <target>"
	@echo ""
	@echo "  build              Debug build"
	@echo "  build-release      Release build"
	@echo ""
	@echo "  test               Run all tests"
	@echo "  fmt                Format code"
	@echo "  fmt-check          Check formatting (no changes)"
	@echo "  lint               Run clippy"
	@echo "  check              fmt-check + lint + test (CI equivalent)"
	@echo ""
	@echo "  run                Run dev server (loads .env, uses config/local.yaml)"
	@echo "  run-release        Run release binary"
	@echo "  health             Check /healthz and /readyz"
	@echo "  metrics            Print /metrics output"
	@echo ""
	@echo "  docker-build       Build Docker image"
	@echo "  docker-up          Start full stack (foreground)"
	@echo "  docker-up-detached Start full stack (background)"
	@echo "  docker-down        Stop full stack"
	@echo "  docker-logs        Tail Ferrox container logs"
	@echo ""
	@echo "  clean              Remove build artifacts"
	@echo ""
