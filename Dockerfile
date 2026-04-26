# syntax=docker/dockerfile:1.6
#
# Multi-stage build that produces a single static musl binary on a
# `scratch` base — final image is just the binary, nothing else.
#
# Build for both linux/amd64 and linux/arm64 in one shot:
#   docker buildx build --platform linux/amd64,linux/arm64 -t … --push .
#
# Cross-compilation uses cargo-zigbuild (zig as the C linker), so the
# builder always runs on the build host's native arch — no QEMU needed,
# both targets cross-compile fast.

ARG RUST_VERSION=1.93
ARG ZIG_VERSION=0.13.0

# ---------- builder (always native to the build host) ----------
FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-bookworm AS builder

ARG TARGETARCH
ARG ZIG_VERSION
WORKDIR /build

# cmake/clang for aws-lc-rs (rustls' default crypto provider — small C code).
# curl + xz-utils to fetch zig.
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake clang curl xz-utils \
    && rm -rf /var/lib/apt/lists/*

# Zig: provides libc + linker for musl cross-compilation. cargo-zigbuild
# wires it into the cargo invocation so any target works without a
# per-target gcc cross-toolchain.
RUN ZIG_ARCH="$(uname -m)" \
    && curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-linux-${ZIG_ARCH}-${ZIG_VERSION}.tar.xz" \
        | tar -xJ -C /opt \
    && ln -s "/opt/zig-linux-${ZIG_ARCH}-${ZIG_VERSION}/zig" /usr/local/bin/zig

RUN cargo install --locked cargo-zigbuild

# Map docker's TARGETARCH to a Rust target triple. Both arches produce
# fully-static musl binaries that run on `scratch`.
RUN case "$TARGETARCH" in \
        amd64) echo "x86_64-unknown-linux-musl"  > /tmp/rust-target ;; \
        arm64) echo "aarch64-unknown-linux-musl" > /tmp/rust-target ;; \
        *) echo "unsupported TARGETARCH=$TARGETARCH"; exit 1 ;; \
    esac \
    && rustup target add "$(cat /tmp/rust-target)"

# Layer 1: dep-cache build with a placeholder src so cargo materializes
# the registry + ~all crates. Subsequent edits to src-rust/ skip this.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src-rust \
    && echo 'fn main() { println!("placeholder"); }' > src-rust/main.rs \
    && TARGET="$(cat /tmp/rust-target)" \
    && cargo zigbuild --release --locked --target "$TARGET" \
    && rm -f src-rust/main.rs "target/$TARGET/release/stremio-nzb-addon" \
    && rm -rf "target/$TARGET/release/deps/stremio_nzb_addon-"* \
              "target/$TARGET/release/.fingerprint/stremio-nzb-addon-"*

# Layer 2: real source. Only this layer rebuilds when src-rust/ changes.
COPY src-rust ./src-rust
COPY assets ./assets
RUN TARGET="$(cat /tmp/rust-target)" \
    && cargo zigbuild --release --locked --target "$TARGET" \
    && cp "target/$TARGET/release/stremio-nzb-addon" /stremio-nzb-addon

# ---------- runtime: scratch ----------
# No shell, no libc, no ca-certs (webpki-roots is bundled in the binary).
# DNS works because Docker / kubelet bind-mount /etc/resolv.conf at start.
FROM scratch AS runtime

COPY --from=builder /stremio-nzb-addon /stremio-nzb-addon

# Inside-container defaults. Mount config.toml + cache from outside.
ENV BIND_ADDR=0.0.0.0 \
    PORT=3000 \
    CONFIG_PATH=/config.toml \
    CACHE_DIR=/cache \
    RUST_LOG=info

EXPOSE 3000

ENTRYPOINT ["/stremio-nzb-addon"]
