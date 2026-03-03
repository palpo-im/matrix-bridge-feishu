# ---- Stage 1: Compute dependency recipe ----
FROM lukemathwalker/cargo-chef:latest-rust-1.93.0-bookworm AS chef
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /workspace

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src/ ./src/
RUN cargo chef prepare --recipe-path recipe.json

# ---- Stage 2: Build dependencies (cached unless Cargo.toml/lock change) ----
FROM chef AS builder
COPY --from=planner /workspace/recipe.json recipe.json
RUN cargo chef cook --profile docker --recipe-path recipe.json

# ---- Stage 3: Build the application ----
COPY Cargo.toml Cargo.lock config src ./
RUN cargo build --profile docker -p matrix-bridge-feishu

# ---- Stage 4: Runtime ----
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -u 1000 appuser

COPY --from=builder /workspace/target/docker/matrix-bridge-feishu /usr/local/bin/matrix-bridge-feishu

RUN mkdir -p /data && chown appuser:appuser /data

USER appuser
WORKDIR /data

EXPOSE 8080 8081

HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:8080/health || exit 1

# Config loading order: CONFIG_PATH env var (if set) -> fallback to config.yaml
CMD ["matrix-bridge-feishu"]
