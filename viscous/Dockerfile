ARG BINARY_SOURCE=source

# ── Stage 1: build (source path) ─────────────────────────────────────────────
FROM rust:1-slim-bookworm AS builder

WORKDIR /build

# Cache workspace dependency compilation separately from source changes.
# All member Cargo.toml files are needed so cargo can resolve the workspace.
COPY Cargo.toml Cargo.lock ./
COPY shasset/Cargo.toml shasset/Cargo.toml
COPY botforge/Cargo.toml botforge/Cargo.toml
COPY viscous/Cargo.toml viscous/Cargo.toml
RUN mkdir -p shasset/src botforge/src viscous/src \
    && echo '' > shasset/src/lib.rs \
    && echo 'fn main(){}' > shasset/src/main.rs \
    && echo 'fn main(){}' > botforge/src/main.rs \
    && echo '' > viscous/src/lib.rs \
    && echo 'fn main(){}' > viscous/src/main.rs \
    && cargo build -p viscous --release --locked \
    && rm -rf shasset/src botforge/src viscous/src

# Build the real binary.
COPY viscous/src viscous/src
RUN touch viscous/src/lib.rs viscous/src/main.rs \
    && cargo build -p viscous --release --locked

# ── Stage 2: source binary ────────────────────────────────────────────────────
FROM scratch AS source
COPY --from=builder /build/target/release/viscous /viscous

# ── Stage 3: prebuilt binary ──────────────────────────────────────────────────
FROM scratch AS prebuilt
COPY .ci-prebuilt/viscous/viscous /viscous

FROM ${BINARY_SOURCE} AS binary-source

# ── Stage 4: runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim

ARG BINARY_SOURCE=source

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=binary-source /viscous /usr/local/bin/viscous

WORKDIR /work

ENTRYPOINT ["viscous"]
CMD ["--help"]
