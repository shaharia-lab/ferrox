.PHONY: build build-release test fmt lint check run run-release clean \
        docker-build docker-up docker-down docker-logs \
        ui-install ui-build ui-dev \
        setup help

# ── Build ──────────────────────────────────────────────────────────────────────

build:
	cargo build --workspace

build-release:
	cargo build --release --workspace

# ── Quality ────────────────────────────────────────────────────────────────────

test:
	cargo test --workspace

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all --check

lint:
	cargo clippy --workspace -- -D warnings

## Run fmt-check + lint + test (same as CI)
check: fmt-check lint test

# ── Run locally ────────────────────────────────────────────────────────────────

## First-time setup: copy .env.example → .env and generate required secrets.
## Safe to re-run — skips if .env already exists.
setup:
	@if [ -f .env ]; then \
		echo ".env already exists, skipping."; \
	else \
		cp .env.example .env; \
		ENC_KEY=$$(openssl rand -hex 32); \
		ADMIN_KEY=$$(openssl rand -hex 20); \
		sed -i "s|^CP_ENCRYPTION_KEY=$$|CP_ENCRYPTION_KEY=$$ENC_KEY|" .env; \
		sed -i "s|^CP_ADMIN_KEY=$$|CP_ADMIN_KEY=$$ADMIN_KEY|" .env; \
		echo "Created .env — fill in at least one provider API key before running."; \
		echo ""; \
		echo "  CP_ENCRYPTION_KEY  auto-generated ✓"; \
		echo "  CP_ADMIN_KEY       auto-generated ✓"; \
		echo "  ANTHROPIC_API_KEY  edit .env and set this"; \
	fi

## Copy minimal config template if local config does not exist yet
ferrox/config/local.yaml:
	cp ferrox/config/config_minimal.yaml ferrox/config/local.yaml
	@echo "Created ferrox/config/local.yaml from config_minimal.yaml — set your API keys in .env before running."

## Run dev server (requires ferrox/config/local.yaml and .env)
run: ferrox/config/local.yaml
	@if [ -f .env ]; then \
		set -a && . ./.env && set +a && LLM_PROXY_CONFIG=ferrox/config/local.yaml cargo run -p ferrox; \
	else \
		LLM_PROXY_CONFIG=ferrox/config/local.yaml cargo run -p ferrox; \
	fi

## Run release binary
run-release: ferrox/config/local.yaml build-release
	@if [ -f .env ]; then \
		set -a && . ./.env && set +a && LLM_PROXY_CONFIG=ferrox/config/local.yaml ./target/release/ferrox; \
	else \
		LLM_PROXY_CONFIG=ferrox/config/local.yaml ./target/release/ferrox; \
	fi

# ── Admin UI (ferrox-cp/ui) ────────────────────────────────────────────────────

## Install npm dependencies for the admin UI
ui-install:
	cd ferrox-cp/ui && npm ci

## Build the admin UI (embeds into the binary via include_dir!)
ui-build: ui-install
	cd ferrox-cp/ui && npm run build

## Start the Vite dev server (proxies /api to localhost:9090)
ui-dev:
	cd ferrox-cp/ui && npm run dev

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
	@echo "  setup              First-time setup: create .env with generated secrets"
	@echo ""
	@echo "  build              Debug build (all workspace members)"
	@echo "  build-release      Release build (all workspace members)"
	@echo ""
	@echo "  test               Run all tests (all workspace members)"
	@echo "  fmt                Format code"
	@echo "  fmt-check          Check formatting (no changes)"
	@echo "  lint               Run clippy"
	@echo "  check              fmt-check + lint + test (CI equivalent)"
	@echo ""
	@echo "  run                Run dev server (loads .env, uses ferrox/config/local.yaml)"
	@echo "  run-release        Run release binary"
	@echo "  health             Check /healthz and /readyz"
	@echo "  metrics            Print /metrics output"
	@echo ""
	@echo "  ui-install         npm ci for the ferrox-cp admin UI"
	@echo "  ui-build           Build admin UI (outputs to ferrox-cp/ui/dist/)"
	@echo "  ui-dev             Vite dev server (proxies /api to localhost:9090)"
	@echo ""
	@echo "  docker-build       Build Docker image"
	@echo "  docker-up          Start full stack (foreground)"
	@echo "  docker-up-detached Start full stack (background)"
	@echo "  docker-down        Stop full stack"
	@echo "  docker-logs        Tail Ferrox container logs"
	@echo ""
	@echo "  clean              Remove build artifacts"
	@echo ""
