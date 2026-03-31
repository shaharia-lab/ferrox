# ── Stage 1: Build ─────────────────────────────────────────────────────────────
FROM rust:1.94-slim-bookworm AS builder

# git required for workspace metadata; no protobuf needed by ferrox-cp
RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Cache dependency compilation layer — copy workspace manifests first, stub src.
COPY Cargo.toml Cargo.lock ./
COPY ferrox/Cargo.toml ferrox/Cargo.toml
COPY ferrox/build.rs ferrox/build.rs
COPY ferrox-cp/Cargo.toml ferrox-cp/Cargo.toml
RUN mkdir -p ferrox/src ferrox-cp/src \
    && echo 'fn main() {}' > ferrox/src/main.rs \
    && echo 'fn main() {}' > ferrox-cp/src/main.rs \
    && cargo build --release -p ferrox-cp \
    && rm -rf ferrox/src ferrox-cp/src

# Copy real source and rebuild only ferrox-cp (deps already cached above)
COPY ferrox-cp/src ./ferrox-cp/src
COPY ferrox-cp/migrations ./ferrox-cp/migrations
RUN touch ferrox-cp/src/main.rs \
    && cargo build --release -p ferrox-cp

# ── Stage 2: Runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    wget \
    && rm -rf /var/lib/apt/lists/*

# Non-root user
RUN groupadd -r ferrox && useradd -r -g ferrox ferrox

WORKDIR /app

COPY --from=builder /build/target/release/ferrox-cp ./ferrox-cp

USER ferrox

EXPOSE 9090

HEALTHCHECK --interval=10s --timeout=3s --start-period=15s --retries=3 \
    CMD wget -qO- http://localhost:9090/healthz || exit 1

ENTRYPOINT ["./ferrox-cp"]
