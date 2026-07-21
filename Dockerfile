# syntax=docker/dockerfile:1

# ─────────────────────────────────────────────────────────────────────────
# Oxigraph Nova — nova_serve container image
#
# Multi-stage build:
#   1. `builder` — compiles the `nova_serve` release binary using the
#      official Rust image (which ships `rustup`, so the nightly toolchain
#      pinned in `rust-toolchain.toml` is picked up and installed
#      automatically — no manual toolchain wrangling needed here).
#   2. final stage — copies only the compiled binary into a minimal Debian
#      base, keeping the resulting image small and free of the Rust
#      toolchain / build dependencies / source tree.
#
# Build:
#   docker build -t oxigraph-nova .
#
# Run (in-memory, bulk-load a dataset mounted at /data/dataset.nt):
#   docker run --rm -p 3030:3030 -v "$PWD/data:/data:ro" oxigraph-nova \
#       --file /data/dataset.nt --bind 0.0.0.0:3030
#
# Run (persistent, WAL-backed store rooted at a mounted volume):
#   docker run --rm -p 3030:3030 -v nova-data:/data oxigraph-nova \
#       --location /data --bind 0.0.0.0:3030
#
# See docker-compose.yml for a ready-to-use Compose setup, and
# `nova_serve --help` (or crates/server/nova-server/src/bin/nova_serve.rs) for the
# full CLI flag reference.
# ─────────────────────────────────────────────────────────────────────────

# ── Stage 1: build ──────────────────────────────────────────────────────
FROM rust:bookworm AS builder

# build-essential/cmake are needed to compile mimalloc's bundled C sources
# (via the libmimalloc-sys build script). clang + lld are required because
# .cargo/config.toml pins `linker = "clang"` and `-fuse-ld=lld` for the
# Linux x86_64/aarch64 targets.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        cmake \
        clang \
        lld \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/oxigraph-nova

# Copy the toolchain pin first so Docker layer caching installs the
# correct (nightly) toolchain before any source is copied in.
COPY rust-toolchain.toml ./rust-toolchain.toml
RUN rustup show

# Copy manifests first to allow Docker to cache the dependency build
# across source-only changes.
COPY Cargo.toml Cargo.lock ./
# core
COPY crates/core/nova-core/Cargo.toml crates/core/nova-core/Cargo.toml
COPY crates/core/nova-query/Cargo.toml crates/core/nova-query/Cargo.toml
COPY crates/core/nova-fulltext/Cargo.toml crates/core/nova-fulltext/Cargo.toml
COPY crates/core/nova-reasoning/Cargo.toml crates/core/nova-reasoning/Cargo.toml
COPY crates/core/nova-shacl/Cargo.toml crates/core/nova-shacl/Cargo.toml
COPY crates/core/nova-cypher/Cargo.toml crates/core/nova-cypher/Cargo.toml
# engines
COPY crates/core/nova-storage/Cargo.toml crates/core/nova-storage/Cargo.toml
COPY crates/engines/nova-engine-memory/Cargo.toml crates/engines/nova-engine-memory/Cargo.toml
COPY crates/engines/nova-engine-louds/Cargo.toml crates/engines/nova-engine-louds/Cargo.toml
COPY crates/engines/nova-engine-ring/Cargo.toml crates/engines/nova-engine-ring/Cargo.toml
# server
COPY crates/server/nova-server/Cargo.toml crates/server/nova-server/Cargo.toml
COPY crates/server/nova-store/Cargo.toml crates/server/nova-store/Cargo.toml
COPY crates/server/nova-cli/Cargo.toml crates/server/nova-cli/Cargo.toml
COPY crates/server/nova-mcp/Cargo.toml crates/server/nova-mcp/Cargo.toml
# bindings
COPY crates/bindings/nova-python/Cargo.toml crates/bindings/nova-python/Cargo.toml
COPY crates/bindings/nova-js/Cargo.toml crates/bindings/nova-js/Cargo.toml
COPY tests/w3c/Cargo.toml tests/w3c/Cargo.toml
COPY benches/Cargo.toml benches/Cargo.toml

# Now copy the full source tree and build the release binary.
COPY . .

RUN cargo build --release --locked -p oxigraph-nova-server --bin nova_serve

# ── Stage 2: runtime ─────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --home-dir /home/nova --shell /usr/sbin/nologin nova

COPY --from=builder /usr/src/oxigraph-nova/target/release/nova_serve /usr/local/bin/nova_serve

# Directory for optional persistent storage (--location) / mounted datasets.
RUN mkdir -p /data && chown nova:nova /data
VOLUME ["/data"]

USER nova
WORKDIR /home/nova

EXPOSE 3030

ENV RUST_LOG=info

ENTRYPOINT ["nova_serve"]
CMD ["--location", "/data", "--bind", "0.0.0.0:3030"]
