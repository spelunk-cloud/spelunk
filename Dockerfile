# spelunk-server — minimal local-scaffold image
#
# Multi-stage build: compile in a Rust builder, copy the binary into a slim
# Debian image. The result is a ~50 MB image with no Rust toolchain overhead.
#
# Build:
#   docker build -t spelunk-server .
#
# This image binds spelunk-server to 127.0.0.1 *inside its own container* by
# default (see CMD below) — that loopback lives in the container's private
# network namespace, so it is NOT reachable via `docker run -p ...` port
# publishing, Docker Desktop host-mode, or from a sibling container's DNS.
# That's intentional, not a bug: spelunk-server refuses to bind a non-loopback
# address over plaintext HTTP, unconditionally, keyed or not (see
# docs/server.md#non-loopback-plaintext-binds-are-refused-no-override), and
# this repo does not ship a proxy to pair with it.
#
# Run (dev, no compose): the server binds loopback only (see above), so a
# sibling container on its own network can't reach it: sibling-container DNS
# resolves to the bridge IP, and nothing listens there. A sidecar has to share
# the server's network namespace instead, then reach it at 127.0.0.1:
#   docker run -d --name spelunk-server -v spelunk-data:/data spelunk-server
#   docker run --rm --network container:spelunk-server curlimages/curl \
#     curl http://127.0.0.1:7777/v1/health
#
# Run (local scaffold, with API key): see docker-compose.yml. It runs this
# image with a persistent volume, wired up with the same
# `--network container:spelunk-server` + 127.0.0.1 pattern above. Nothing
# more; it does not publish a host-reachable port.
#
# For a team-reachable deployment, don't containerize this at all: run the
# binary bare-metal/systemd on a host, with your own TLS terminator (nginx,
# Caddy, ...) in front of the same loopback bind on that host. See
# docs/self-hosting.md — that's the recommended path, since a container's
# loopback can't be handed to a same-host proxy the way a bare-metal
# process's can.

# ── Stage 1: workspace skeleton ───────────────────────────────────────────────
# Everything the dependency cache below needs to load the workspace, and nothing
# else: every member's manifest, plus a placeholder source for the targets each
# one declares. Reducing the real tree here, rather than naming each crate in a
# COPY, is what keeps adding a workspace member from breaking this image. Docker
# cannot copy `crates/*/Cargo.toml` while preserving the directory each came
# from, so the tree is copied whole and pruned.
FROM rust:1.98.0-slim AS skeleton

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN find crates -mindepth 2 -maxdepth 2 ! -name Cargo.toml -exec rm -rf {} + && \
    for c in crates/*/; do \
        mkdir -p "$c/src" && \
        : > "$c/src/lib.rs" && \
        echo 'fn main(){}' > "$c/src/main.rs"; \
    done

# ── Stage 2: build ────────────────────────────────────────────────────────────
FROM rust:1.98.0-slim AS builder

WORKDIR /build

# System build deps the slim image lacks: a C/C++ toolchain for tokenizers'
# esaxx-rs build script (embed-native default), and libdbus-1-dev to satisfy
# libdbus-sys's build script (pulled via keyring's sync-secret-service backend).
# Build-time only — the linker strips the unused lib, so the runtime image
# needs no dbus package.
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential pkg-config libdbus-1-dev \
    && rm -rf /var/lib/apt/lists/*

# Cache dependency compilation separately from source changes. This is a
# virtual Cargo workspace (no root package), so prime the cache from the
# skeleton above; the heavy third-party deps (candle, etc.) then land in a layer
# that only busts when a manifest or Cargo.lock changes, because that is all the
# skeleton contains. Every member manifest must be present and its declared
# target source must exist, or cargo refuses to load the workspace, even members
# the server bin doesn't depend on.
COPY --from=skeleton /build /build
RUN cargo build --release --bin spelunk-server && \
    rm -rf crates/*/src

# Now copy the real source and build properly. BuildKit normalizes COPY mtimes
# to a constant OLDER than the cached placeholder artifacts, so cargo's
# freshness check would reuse the placeholder binary. `touch` every crate
# source so the real build supersedes the cache.
COPY . .
RUN find crates -name '*.rs' -exec touch {} + && \
    cargo build --release --bin spelunk-server

# ── Stage 3: runtime ──────────────────────────────────────────────────────────
FROM debian:trixie-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# `-m -d /data` gives the service user a real home at the same path the
# volume is mounted, so any `$HOME`-relative path a dependency resolves stays
# writable; `useradd -m` also creates /data pre-owned by spelunk, so no
# separate chown is needed. WORKDIR after useradd picks up that existing dir.
RUN useradd -r -m -d /data -s /bin/false spelunk
WORKDIR /data

COPY --from=builder /build/target/release/spelunk-server /usr/local/bin/spelunk-server

# Primary fix: point the embedder's model cache at the persistent /data
# volume instead of the default $HOME/.local/share resolution. Without this,
# a fresh container re-downloads the ~339 MB model into the container layer
# on every `docker rm`/recreate even once $HOME itself is writable (see
# useradd above).
ENV XDG_DATA_HOME=/data

USER spelunk

EXPOSE 7777

ENTRYPOINT ["/usr/local/bin/spelunk-server"]
# Bind loopback — the binary's own default, and the only bind this image
# supports. spelunk-server refuses to bind a non-loopback address over
# plaintext HTTP unconditionally, keyed or not (see
# docs/server.md#non-loopback-plaintext-binds-are-refused-no-override), so a
# `--host 0.0.0.0` override here would just make the server refuse to start —
# this image ships no proxy to pair with it. For a deployment that needs to be
# reachable off-host, don't override this; run bare-metal/systemd instead (see
# docs/self-hosting.md), where a same-host reverse proxy can front the
# server's loopback bind directly.
CMD ["--host", "127.0.0.1", "--db", "/data/spelunk.db"]
