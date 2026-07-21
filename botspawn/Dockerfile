ARG BINARY_SOURCE=source

# ── Stage 1: build (source path) ─────────────────────────────────────────────
FROM rust:1-slim-bookworm AS builder

WORKDIR /build

# Cache workspace dependency compilation separately from source changes.
# botspawn has no workspace-internal dependencies; only the workspace manifest
# and botspawn's own Cargo.toml are needed to warm the dep cache.
COPY Cargo.toml Cargo.lock ./
COPY botspawn/Cargo.toml botspawn/Cargo.toml
RUN mkdir -p botspawn/src \
    && echo 'fn main(){}' > botspawn/src/main.rs \
    && cargo build -p botspawn --release --locked \
    && rm -rf botspawn/src

# Build the real binary.
COPY botspawn/src botspawn/src
RUN touch botspawn/src/main.rs \
    && cargo build -p botspawn --release --locked

# ── Stage 2: source binary ────────────────────────────────────────────────────
FROM scratch AS source
COPY --from=builder /build/target/release/botspawn /botspawn

# ── Stage 3: prebuilt binary ──────────────────────────────────────────────────
FROM scratch AS prebuilt
COPY .ci-prebuilt/botspawn/botspawn /botspawn

FROM ${BINARY_SOURCE} AS binary-source

# ── Stage 4: runtime ──────────────────────────────────────────────────────────
# botspawn is a thin launcher; it only needs the binary itself plus the
# Docker CLI so it can invoke botforge inside its container.
#
# hadolint ignore=DL3008
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        docker.io \
    && rm -rf /var/lib/apt/lists/*

COPY --from=binary-source /botspawn /usr/local/bin/botspawn

ENTRYPOINT ["/usr/local/bin/botspawn"]
CMD ["--help"]
