ARG BINARY_SOURCE=source

# ── Stage 1: build (source path) ─────────────────────────────────────────────
FROM rust:1-slim-bookworm AS builder

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY shasset/Cargo.toml shasset/Cargo.toml
COPY botforge/Cargo.toml botforge/Cargo.toml
COPY botspawn/Cargo.toml botspawn/Cargo.toml
RUN mkdir -p botspawn/src \
    && echo 'fn main(){}' > botspawn/src/main.rs \
    && cargo build -p botspawn --release --locked \
    && rm -rf botspawn/src

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
FROM debian:bookworm-slim

ARG BINARY_SOURCE=source

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=binary-source /botspawn /usr/local/bin/botspawn

WORKDIR /work

ENTRYPOINT ["botspawn"]
CMD ["--help"]
