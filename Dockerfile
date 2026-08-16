# =============================================================================
# gasket Dockerfile - Multi-stage build (Rust gateway + Vue frontend)
# =============================================================================
# Produces a single image running `gasket-gateway` on port 3000, serving the
# built Vue frontend from /app/web/dist.
#
# Usage:
#   docker build -t gasket .
#   docker run -d -p 3000:3000 \
#     -e GASKET_LLM_BASE_URL=... -e GASKET_LLM_KEY=... \
#     -e GASKET_LLM_MODEL=... -e GASKET_LLM_API=openai \
#     gasket
# =============================================================================

# -----------------------------------------------------------------------------
# Stage 1: Frontend builder (Vue 3 + Vite → dist/)
# -----------------------------------------------------------------------------
FROM node:20-bookworm-slim AS web-builder

WORKDIR /web

# Install pnpm
RUN npm install -g pnpm@9

# Copy lockfile + package.json first for cache
COPY web/package.json web/pnpm-lock.yaml ./

RUN pnpm install --frozen-lockfile

# Copy the rest of the frontend source and build
COPY web/ ./
RUN pnpm build

# -----------------------------------------------------------------------------
# Stage 2: Rust builder (gasket-gateway binary)
# -----------------------------------------------------------------------------
FROM rust:1.82-bookworm AS rust-builder

WORKDIR /build

# Copy workspace root files for dependency caching
COPY gasket/Cargo.toml gasket/Cargo.lock ./

# Copy all workspace member Cargo.toml files
COPY gasket/gasket-core/Cargo.toml ./gasket-core/
COPY gasket/gasket-host/Cargo.toml ./gasket-host/
COPY gasket/gasket-cli/Cargo.toml ./gasket-cli/
COPY gasket/gasket-ext/Cargo.toml ./gasket-ext/
COPY gasket/gasket-gateway/Cargo.toml ./gasket-gateway/

# Create dummy source files so cargo can build dependencies layer
RUN mkdir -p \
        gasket-core/src \
        gasket-host/src \
        gasket-cli/src \
        gasket-ext/src \
        gasket-gateway/src && \
    echo "pub fn dummy() {}" > gasket-core/src/lib.rs && \
    echo "pub fn dummy() {}" > gasket-host/src/lib.rs && \
    echo "fn main() {}" > gasket-cli/src/main.rs && \
    echo "pub fn dummy() {}" > gasket-ext/src/lib.rs && \
    echo "fn main() {}" > gasket-gateway/src/main.rs && \
    cargo build --release --bin gasket-gateway --all-features && \
    rm -rf \
        gasket-core/src \
        gasket-host/src \
        gasket-cli/src \
        gasket-ext/src \
        gasket-gateway/src

# Copy actual source code
COPY gasket/gasket-core/src ./gasket-core/src
COPY gasket/gasket-host/src ./gasket-host/src
COPY gasket/gasket-cli/src ./gasket-cli/src
COPY gasket/gasket-ext/src ./gasket-ext/src
COPY gasket/gasket-gateway/src ./gasket-gateway/src

# Touch source files to invalidate cargo cache and rebuild
RUN touch \
        gasket-core/src/lib.rs \
        gasket-host/src/lib.rs \
        gasket-cli/src/main.rs \
        gasket-ext/src/lib.rs \
        gasket-gateway/src/main.rs && \
    cargo build --release --bin gasket-gateway --all-features

# -----------------------------------------------------------------------------
# Stage 3: Runtime
# -----------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# Install runtime dependencies
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        ca-certificates \
        libssl3 && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy the gateway binary
COPY --from=rust-builder /build/target/release/gasket-gateway /usr/local/bin/gasket-gateway

# Copy the built frontend
COPY --from=web-builder /web/dist /app/web/dist

# Create config directory
RUN mkdir -p /root/.gasket

# Gateway default port
EXPOSE 3000

# Point the gateway at the bundled frontend
ENV GASKET_GATEWAY_STATIC_DIR=/app/web/dist

ENTRYPOINT ["gasket-gateway"]
CMD []
