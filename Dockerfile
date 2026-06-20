# syntax=docker/dockerfile:1.7
# Multi-stage Dockerfile for the A2W server.
#
# Stage 1 (`builder`) — compile every workspace binary on a Rust toolchain.
# Stage 2 (`runtime`) — copy only the compiled artifact onto a tiny, non-root
# debian-slim image. The runtime image has no toolchain, no headers, no shell
# scripts — only the binary, a CA-cert bundle for outbound HTTPS, and the
# `tini` init for clean PID-1 signal handling.

# ---------------------------------------------------------------------------
# Builder
# ---------------------------------------------------------------------------
FROM rust:1.91-slim-bookworm AS builder
WORKDIR /workspace

# System headers needed by sqlx / aes-gcm / extism transitive deps.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# Copy manifests first so cargo's dep cache can be reused across edits.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# Build only the two production binaries.
ENV CARGO_INCREMENTAL=0 \
    CARGO_TERM_COLOR=never \
    RUST_BACKTRACE=1
RUN cargo build --release -p a2w-server -p a2w-mcp

# ---------------------------------------------------------------------------
# Runtime
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime
ARG APP_USER=a2w
ARG APP_UID=10001

# Minimal runtime: TLS roots for outbound HTTPS + `tini` init.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tini \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --system --gid ${APP_UID} ${APP_USER} \
 && useradd  --system --uid ${APP_UID} --gid ${APP_UID} \
        --no-create-home --shell /usr/sbin/nologin ${APP_USER} \
 && mkdir -p /var/lib/a2w \
 && chown -R ${APP_USER}:${APP_USER} /var/lib/a2w

# Copy artifacts. `a2w-mcp` is shipped alongside `a2w-server` so the MCP
# stdio command allowlist can permit `a2w-mcp` against the same image.
COPY --from=builder /workspace/target/release/a2w-server /usr/local/bin/a2w-server
COPY --from=builder /workspace/target/release/a2w-mcp    /usr/local/bin/a2w-mcp

USER ${APP_USER}
WORKDIR /var/lib/a2w

# Sensible defaults; override at deploy time.
ENV A2W_BIND=0.0.0.0:8080 \
    A2W_DB_URL=sqlite:///var/lib/a2w/a2w.db?mode=rwc \
    A2W_LOG_JSON=true \
    RUST_LOG=info

EXPOSE 8080

# `tini` reaps zombie children spawned by the MCP node (which forks child
# processes for `wf_*` stdio servers).
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["a2w-server"]

# Healthcheck — use the readiness probe so an unreachable DB drains the pod.
HEALTHCHECK --interval=15s --timeout=3s --start-period=10s --retries=3 \
  CMD wget --spider --quiet http://127.0.0.1:8080/ready || exit 1
