# ---- Build stage ----
FROM rust:1-bookworm AS builder
WORKDIR /app

# Cache dependencies first.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
    && echo "" > src/lib.rs \
    && cargo build --release --bin hyperbot || true \
    && rm -rf src

# Build the real sources.
COPY migrations ./migrations
COPY src ./src
RUN cargo build --release --bin hyperbot

# ---- Runtime stage ----
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Run as a non-root user.
RUN useradd --create-home --uid 10001 hyperbot
WORKDIR /app

COPY --from=builder /app/target/release/hyperbot /usr/local/bin/hyperbot
COPY config.example.toml ./config.example.toml

USER hyperbot
ENV HYPERBOT_CONFIG=/app/config.toml \
    RUST_LOG=info

ENTRYPOINT ["hyperbot"]
