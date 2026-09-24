# ── Stage 1: Build ─────────────────────────────────────────────────────────────
FROM rust:1.98-slim-bookworm@sha256:ff521445a372125ed4f76e1453a1f8098f2d05332d1601d30db1c1f62757e730 AS builder

# protobuf-compiler required by opentelemetry-otlp/tonic build script
# git required by build.rs (git rev-parse for version embedding)
RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    git \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Cache dependency compilation layer — copy workspace manifests + build script first, stub src.
# build.rs runs here too; GIT_SHA will be "unknown" for the dep-cache layer, which is fine.
COPY Cargo.toml Cargo.lock ./
COPY ferrox/Cargo.toml ferrox/Cargo.toml
COPY ferrox/build.rs ferrox/build.rs
COPY ferrox-cp/Cargo.toml ferrox-cp/Cargo.toml
COPY ferrox-providers/Cargo.toml ferrox-providers/Cargo.toml
RUN mkdir -p ferrox/src ferrox-cp/src ferrox-providers/src \
    && echo 'fn main() {}' > ferrox/src/main.rs \
    && echo 'fn main() {}' > ferrox-cp/src/main.rs \
    && : > ferrox-providers/src/lib.rs \
    && cargo build --release -p ferrox \
    && rm -rf ferrox/src ferrox-cp/src ferrox-providers/src

# Copy real source and rebuild only ferrox (deps already cached above)
COPY ferrox-providers/src ./ferrox-providers/src
COPY ferrox/src ./ferrox/src
RUN touch ferrox/src/main.rs ferrox-providers/src/lib.rs \
    && cargo build --release -p ferrox

# ── Stage 2: Runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    wget \
    && rm -rf /var/lib/apt/lists/*

# Non-root user
RUN groupadd -r ferrox && useradd -r -g ferrox ferrox

WORKDIR /app

COPY --from=builder /build/target/release/ferrox ./ferrox
COPY ferrox/config/config.yaml ./config/config.yaml

USER ferrox

EXPOSE 8080

HEALTHCHECK --interval=10s --timeout=3s --start-period=15s --retries=3 \
    CMD wget -qO- http://localhost:8080/healthz || exit 1

ENTRYPOINT ["./ferrox"]
