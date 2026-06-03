ARG BINARY_SOURCE=source

# ── Stage 1: build (source path) ─────────────────────────────────────────────
FROM rust:1-slim-bookworm AS builder

WORKDIR /build

# Cache workspace dependency compilation separately from source changes.
# All member Cargo.toml files are needed so cargo can resolve the workspace.
COPY Cargo.toml Cargo.lock ./
COPY shasset/Cargo.toml shasset/Cargo.toml
COPY botforge/Cargo.toml botforge/Cargo.toml
RUN mkdir -p shasset/src \
    && echo 'fn main(){}' > shasset/src/main.rs \
    && echo '' > shasset/src/lib.rs \
    && cargo build -p shasset --release --locked \
    && rm -rf shasset/src

# Build the real binary.
COPY shasset/src shasset/src
RUN touch shasset/src/main.rs shasset/src/lib.rs \
    && cargo build -p shasset --release --locked

# ── Stage 2: source binary ────────────────────────────────────────────────────
FROM scratch AS source
COPY --from=builder /build/target/release/shasset /shasset

# ── Stage 3: prebuilt binary ──────────────────────────────────────────────────
FROM scratch AS prebuilt
COPY .ci-prebuilt/shasset/shasset /shasset

FROM ${BINARY_SOURCE} AS binary-source

# ── Stage 4: runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim

ARG BINARY_SOURCE=source

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=binary-source /shasset /usr/local/bin/shasset

WORKDIR /work

ENTRYPOINT ["shasset"]
CMD ["--help"]
