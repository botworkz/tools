# ── Stage 1: build ────────────────────────────────────────────────────────────
FROM rust:1-slim-bookworm AS builder

WORKDIR /build

# Cache dependency compilation separately from source changes.
COPY shasset/Cargo.toml shasset/Cargo.lock ./
RUN mkdir src && echo 'fn main(){}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

# Build the real binary.
COPY shasset/src ./src
RUN touch src/main.rs \
    && cargo build --release --locked

# ── Stage 2: runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/shasset /usr/local/bin/shasset

WORKDIR /work

ENTRYPOINT ["shasset"]
CMD ["--help"]
