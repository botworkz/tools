ARG BINARY_SOURCE=source

# ── Stage 1: build (source path) ─────────────────────────────────────────────
FROM rust:1-slim-bookworm AS builder

WORKDIR /build

# Cache workspace dependency compilation separately from source changes.
# All member Cargo.toml files are needed so cargo can resolve the workspace.
COPY Cargo.toml Cargo.lock ./
COPY shasset/Cargo.toml shasset/Cargo.toml
COPY botforge/Cargo.toml botforge/Cargo.toml
RUN mkdir -p shasset/src botforge/src \
    && echo '' > shasset/src/lib.rs \
    && echo 'fn main(){}' > shasset/src/main.rs \
    && echo 'fn main(){}' > botforge/src/main.rs \
    && cargo build -p botforge --release --locked \
    && rm -rf shasset/src botforge/src

# Build the real binary.
COPY shasset/src shasset/src
COPY botforge/src botforge/src
RUN touch shasset/src/lib.rs shasset/src/main.rs botforge/src/main.rs \
    && cargo build -p botforge --release --locked

# ── Stage 2: source binary ────────────────────────────────────────────────────
FROM scratch AS source
COPY --from=builder /build/target/release/botforge /botforge

# ── Stage 3: prebuilt binary ──────────────────────────────────────────────────
FROM scratch AS prebuilt
COPY .ci-prebuilt/botforge/botforge /botforge

FROM ${BINARY_SOURCE} AS binary-source

# ── Stage 4: runtime (batteries-included) ────────────────────────────────────
# botforge orchestrates external tools by shelling out; bake the full toolchain
# so subcommands work reproducibly inside the container without host deps.
FROM debian:trixie-slim

ARG BINARY_SOURCE=source

# Install orchestration toolchain:
#   qemu-system-x86 / qemu-utils  — qemu-system-x86_64, qemu-img
#   xorriso / genisoimage         — ISO building (cidata seed for `botforge test`)
#   libnss-wrapper                — uid/gid passthrough support in entrypoint
#   ca-certificates               — TLS roots
#   cloud-image-utils / curl / jq — ancillary runtime utilities
#
# openssh-client is intentionally absent: botforge uses a pure-Rust SSH/SFTP
# implementation (russh + russh-sftp) and no longer shells out to ssh/scp/ssh-keygen.
#
# hadolint ignore=DL3008
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        cloud-image-utils \
        curl \
        genisoimage \
        jq \
        libnss-wrapper \
        qemu-system-x86 \
        qemu-utils \
        xorriso \
    && rm -rf /var/lib/apt/lists/*

COPY --from=binary-source /botforge /usr/local/bin/botforge
COPY --chmod=0755 botforge/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh

WORKDIR /work

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
CMD ["--help"]
