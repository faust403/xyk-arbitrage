# syntax=docker/dockerfile:1.7
# Multi-stage build for the Phase −1 scout. Targets EU-West Railway region.
# Match `rust-toolchain.toml` (1.86.0) so the lock file resolves identically.

# ============================================================
# Stage 1: builder
# ============================================================
FROM rust:1.88-slim-bookworm AS builder

# LaserStream feature pulls yellowstone-grpc + tonic + prost. tonic-build
# needs `protoc` (`protobuf-compiler`). The transitive `protobuf-src` build
# script otherwise tries to compile protoc from source — which needs
# `make`, `zlib1g-dev`, and the full autotools chain. We sidestep that by
# setting `PROTOC=/usr/bin/protoc` so cargo uses the system binary.
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
        libssl-dev \
        ca-certificates \
        protobuf-compiler \
        cmake \
        make \
        zlib1g-dev \
        gcc \
        g++ \
        && rm -rf /var/lib/apt/lists/*

# Force cargo's protobuf-src crate to use the system protoc instead of
# bootstrapping a vendored copy. `PROTOC_NO_VENDOR=1` is the modern flag;
# we also set `PROTOC` explicitly as a belt-and-suspenders.
ENV PROTOC=/usr/bin/protoc \
    PROTOC_NO_VENDOR=1

WORKDIR /build

# Copy manifests + sources together. (We could split for layer caching but
# Cargo's workspace makes the deps-only stub trick fragile here.)
COPY Cargo.toml ./
COPY rust-toolchain.toml ./
# Cargo.lock is optional: if present we honour it, otherwise cargo regenerates.
COPY Cargo.lock* ./
COPY crates/ ./crates/

# Release build of the scout binary with the LaserStream gRPC feature
# enabled. The arb-stream crate's `laserstream` feature pulls in
# yellowstone-grpc-client + tonic; the Dockerfile installs the native
# build-deps (protoc, cmake, gcc) those crates need.
#
# To skip LaserStream and ship a smaller image (~30% lighter), drop
# `--features arb-stream/laserstream` here AND remove protobuf-compiler
# + cmake + g++ from the apt install above.
# LaserStream feature ENABLED — laserstream.rs is now bound to the
# `subscribe()`-then-send pattern that matches yellowstone-grpc-client
# v1.9.0+solana.1.16.1 (pinned in the workspace Cargo.toml). System
# `protoc` from `protobuf-compiler` handles the proto-build step;
# `PROTOC` env above forces the build script away from vendoring.
RUN cargo build --release --bin arb-scout \
        -p arb-scout --features laserstream

# ============================================================
# Stage 2: runtime — slim Debian, non-root user
# ============================================================
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        libssl3 \
        procps \
        && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --shell /usr/sbin/nologin --uid 10001 app

WORKDIR /app
RUN mkdir -p /app/logs && chown -R app:app /app
USER app

COPY --from=builder --chown=app:app /build/target/release/arb-scout /app/arb-scout
COPY --chown=app:app config/ /app/config/

# NOTE: do NOT use Docker's VOLUME directive — Railway rejects it. The
# `/app/logs` mount is configured in the Railway dashboard (Settings →
# Volumes → Mount path: /app/logs). The directory is created above with
# `mkdir -p /app/logs` so the binary can write to it whether or not a
# Railway volume is attached.

# Process-presence health check (no HTTP server in v1).
HEALTHCHECK --interval=30s --timeout=10s --start-period=15s --retries=3 \
    CMD pgrep -f arb-scout || exit 1

# Default config path. Railway's `startCommand` in railway.json overrides this.
ENTRYPOINT ["/app/arb-scout"]
CMD ["/app/config/scout-pools.toml"]
