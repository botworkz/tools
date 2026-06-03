# ── Stage 1: build ────────────────────────────────────────────────────────────
FROM rust:1-slim-bookworm AS builder

WORKDIR /build

# Cache dependency compilation separately from source changes.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main(){}' > src/main.rs && echo '' > src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

# Build the real binary.
COPY src ./src
RUN touch src/main.rs src/lib.rs \
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
