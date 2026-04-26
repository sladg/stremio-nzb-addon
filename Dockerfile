# syntax=docker/dockerfile:1.6
#
# Multi-stage build for the Rust addon binary.
# - Builder: rust:1.93-bookworm with cached cargo registry + target dir.
# - Runtime: debian:bookworm-slim (~80 MB final image).
#
# Why debian-slim and not distroless: keeps a shell + curl available for
# debugging via `docker exec` and lets compose health-check via curl.

ARG RUST_VERSION=1.93

# ---------- builder ----------
FROM rust:${RUST_VERSION}-bookworm AS builder

WORKDIR /build

# `aws-lc-rs` (rustls' default crypto provider) builds a small amount of C
# at compile time. cmake + a C compiler cover everything it needs.
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake clang \
    && rm -rf /var/lib/apt/lists/*

# Layer 1: cache dependency builds. We copy only the manifests, then build
# a placeholder binary so cargo materializes the registry + ~all crates.
# Subsequent edits to src-rust/ skip this layer entirely.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src-rust \
    && echo 'fn main() { println!("dependency-cache placeholder"); }' > src-rust/main.rs \
    && cargo build --release --locked \
    && rm -f src-rust/main.rs target/release/stremio-nzb-addon \
    && rm -rf target/release/deps/stremio_nzb_addon-* target/release/.fingerprint/stremio-nzb-addon-*

# Layer 2: real source. Only this layer rebuilds when src-rust/ changes.
COPY src-rust ./src-rust
RUN cargo build --release --locked \
    && strip target/release/stremio-nzb-addon

# ---------- runtime ----------
FROM debian:bookworm-slim AS runtime

# ca-certificates for HTTPS to indexers (reqwest/rustls uses webpki-roots
# bundled into the binary, but having the system bundle on the box is
# cheap insurance for any future http client we might add).
# curl present for the compose health-check.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl tini \
    && rm -rf /var/lib/apt/lists/*

# Non-root user. Avoids files owned-by-root showing up in bind mounts on
# the host. UID 1000 matches typical desktop accounts on Linux/macOS.
ARG APP_UID=1000
ARG APP_GID=1000
RUN groupadd --system --gid ${APP_GID} addon \
    && useradd  --system --gid ${APP_GID} --uid ${APP_UID} \
       --home-dir /app --shell /usr/sbin/nologin addon \
    && mkdir -p /app /app/cache \
    && chown -R addon:addon /app

WORKDIR /app
COPY --from=builder --chown=addon:addon /build/target/release/stremio-nzb-addon /app/stremio-nzb-addon

USER addon

# Inside-container defaults. Override via compose env if you want.
#   BIND_ADDR  — 0.0.0.0 because the container's loopback isn't reachable
#                from the host. Compose maps the published port back to host.
#   CACHE_DIR  — /app/cache; pair with a volume mount in compose.
#   CONFIG_PATH — /app/config.toml; mount the host file in compose.
ENV BIND_ADDR=0.0.0.0 \
    PORT=3000 \
    CONFIG_PATH=/app/config.toml \
    CACHE_DIR=/app/cache \
    RUST_LOG=info

EXPOSE 3000

# `tini` reaps zombies + forwards signals so SIGTERM from `docker stop`
# actually shuts the binary down cleanly.
ENTRYPOINT ["/usr/bin/tini", "--", "/app/stremio-nzb-addon"]
