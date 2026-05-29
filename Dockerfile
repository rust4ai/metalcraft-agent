FROM rust:1.91-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./

# Cache dependency build
RUN mkdir -p src/bin && \
    echo "fn main() {}" > src/main.rs && \
    echo "fn main() {}" > src/bin/metalcraft-daemon.rs && \
    echo "" > src/lib.rs && \
    cargo build --release --bin metalcraft-daemon 2>/dev/null || true && \
    rm -rf src

COPY src ./src
COPY seed ./seed
RUN touch src/main.rs src/lib.rs src/bin/metalcraft-daemon.rs && \
    cargo build --release --bin metalcraft-daemon

# ── Runtime ──────────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/metalcraft-daemon /usr/local/bin/metalcraft-daemon
COPY seed /opt/metalcraft/seed

ENV RUST_LOG=info

CMD ["metalcraft-daemon", "--auto-approve"]
