# =============================================================================
# conga Dockerfile - Multi-stage build (Rust gateway + Vue frontend)
# =============================================================================
# Produces a single image running `conga-gateway` on port 3000, serving the
# built Vue frontend from /app/web/dist.
#
# Usage:
#   docker build -t conga .
#   docker run -d -p 3000:3000 \
#     -e CONGA_LLM_BASE_URL=... -e CONGA_LLM_KEY=... \
#     -e CONGA_LLM_MODEL=... -e CONGA_LLM_API=openai \
#     conga
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
# Stage 2: Rust builder (conga-gateway binary)
# -----------------------------------------------------------------------------
FROM rust:1.82-bookworm AS rust-builder

WORKDIR /build

# Copy workspace root files for dependency caching
COPY conga/Cargo.toml conga/Cargo.lock ./

# Copy all workspace member Cargo.toml files
COPY conga/conga/Cargo.toml ./conga/
COPY conga/conga-host/Cargo.toml ./conga-host/
COPY conga/conga-cli/Cargo.toml ./conga-cli/
COPY conga/conga-ext/Cargo.toml ./conga-ext/
COPY conga/conga-gateway/Cargo.toml ./conga-gateway/

# Create dummy source files so cargo can build dependencies layer
RUN mkdir -p \
        conga/src \
        conga-host/src \
        conga-cli/src \
        conga-ext/src \
        conga-gateway/src && \
    echo "pub fn dummy() {}" > conga/src/lib.rs && \
    echo "pub fn dummy() {}" > conga-host/src/lib.rs && \
    echo "fn main() {}" > conga-cli/src/main.rs && \
    echo "pub fn dummy() {}" > conga-ext/src/lib.rs && \
    echo "fn main() {}" > conga-gateway/src/main.rs && \
    cargo build --release --bin conga-gateway --all-features && \
    rm -rf \
        conga/src \
        conga-host/src \
        conga-cli/src \
        conga-ext/src \
        conga-gateway/src

# Copy actual source code
COPY conga/conga/src ./conga/src
COPY conga/conga-host/src ./conga-host/src
COPY conga/conga-cli/src ./conga-cli/src
COPY conga/conga-ext/src ./conga-ext/src
COPY conga/conga-gateway/src ./conga-gateway/src

# Touch source files to invalidate cargo cache and rebuild
RUN touch \
        conga/src/lib.rs \
        conga-host/src/lib.rs \
        conga-cli/src/main.rs \
        conga-ext/src/lib.rs \
        conga-gateway/src/main.rs && \
    cargo build --release --bin conga-gateway --all-features

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
COPY --from=rust-builder /build/target/release/conga-gateway /usr/local/bin/conga-gateway

# Copy the built frontend
COPY --from=web-builder /web/dist /app/web/dist

# Create config directory
RUN mkdir -p /root/.conga

# Gateway default port
EXPOSE 3000

# Point the gateway at the bundled frontend
ENV CONGA_GATEWAY_STATIC_DIR=/app/web/dist

ENTRYPOINT ["conga-gateway"]
CMD []
