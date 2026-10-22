# syntax=docker/dockerfile:1

# ---------------------------------------------------------------------------
# Builder
# ---------------------------------------------------------------------------
FROM rust:1.96-slim-bookworm AS builder

# aws-lc-sys (rustls crypto backend for reqwest and lettre) and the bundled
# SQLite in rusqlite are compiled from C. No OpenSSL is needed.
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential cmake perl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Build dependencies against a dummy main so this layer stays cached until
# Cargo.toml / Cargo.lock change.
COPY Cargo.toml Cargo.lock ./
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

# Build the real binary.
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    touch src/main.rs \
    && cargo build --release --locked \
    && cp target/release/gen /usr/local/bin/gen

# ---------------------------------------------------------------------------
# Runtime
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --user-group --home-dir /app --shell /usr/sbin/nologin gen \
    && mkdir -p /data \
    && chown gen:gen /data

WORKDIR /app

COPY --from=builder /usr/local/bin/gen /usr/local/bin/gen
COPY templates ./templates
COPY static ./static
COPY deploy/gen.docker.toml ./config/gen.toml

ENV GEN_CONFIG=/app/config/gen.toml \
    RUST_LOG=info

VOLUME ["/data"]
EXPOSE 8080
USER gen

# The slim image has no curl: use bash's /dev/tcp to request /health and
# check for a 200 status line.
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD ["bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/8080 && printf 'GET /health HTTP/1.0\\r\\nHost: localhost\\r\\n\\r\\n' >&3 && head -n1 <&3 | grep -q ' 200 '"]

CMD ["gen", "serve"]
