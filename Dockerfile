# Build stage
FROM rust:1.93 as builder

WORKDIR /app

# Copy cargo files
COPY Cargo.toml Cargo.lock example-config.yaml ./

# Create dummy main.rs to cache dependencies
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release && rm -rf src

# Copy source code
COPY src ./src

# Build the application
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Create app user
RUN useradd -r -s /bin/false matrix

# Copy binary from builder
COPY --from=builder /app/target/release/matrix-appservice-feishu /usr/local/bin/

# Create data directory
RUN mkdir -p /data && chown matrix:matrix /data

# Switch to non-root user
USER matrix

# Expose ports
EXPOSE 8080 8081

# Health check
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:8080/health || exit 1

# Run the application
CMD ["matrix-appservice-feishu", "-c", "/config/config.yaml"]