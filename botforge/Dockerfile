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
FROM debian:bookworm-slim

ARG BINARY_SOURCE=source

# Install orchestration toolchain:
#   qemu-system-x86 / qemu-utils  — qemu-system-x86_64, qemu-img
#   libguestfs-tools / linux-image-amd64 — retained for potential future use;
#                                 no longer required by the modern build path
#                                 (potential follow-up: remove if confirmed unused)
#   xorriso / genisoimage         — ISO building (cidata seed for `botforge test`)
#   openssh-client                — ssh, scp, ssh-keygen
#   ca-certificates               — TLS roots
#
# hadolint ignore=DL3008
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        cloud-image-utils \
        curl \
        genisoimage \
        jq \
        libguestfs-tools \
        libnss-wrapper \
        linux-image-amd64 \
        openssh-client \
        qemu-system-x86 \
        qemu-utils \
        xorriso \
    && rm -rf /var/lib/apt/lists/*

# libguestfs needs a writable cache for its supermin appliance. /tmp/.cache is
# the path docker-entrypoint.sh hands out via XDG_CACHE_HOME. Pre-create so
# unprivileged uid/gid runs (nss-wrapped) can write without ENOENT surprises.
RUN mkdir -p /tmp/.cache && chmod 1777 /tmp/.cache

# Direct backend avoids needing libvirtd; the host still supplies /dev/kvm when
# available for hardware acceleration, otherwise libguestfs falls back to TCG.
ENV LIBGUESTFS_BACKEND=direct

COPY --from=binary-source /botforge /usr/local/bin/botforge
COPY --chmod=0755 botforge/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh

WORKDIR /work

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
CMD ["--help"]
