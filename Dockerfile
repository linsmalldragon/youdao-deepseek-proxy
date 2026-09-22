# syntax=docker/dockerfile:1.7

# ---------------------------------------------------------------- build ----
# Cross-compile: the builder stage runs on the host platform (BUILDPLATFORM)
# and produces a binary for TARGETARCH, so one Dockerfile serves both
# linux/amd64 and linux/arm64 under buildx.
FROM --platform=$BUILDPLATFORM rust:1 AS builder

ARG TARGETARCH

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
RUN cargo fetch

# Static musl binaries: the official rust image ships musl-tools, so the
# C parts of ring cross-compile against musl without any host glibc
# headers. No apt packages needed for either architecture.
RUN case "${TARGETARCH}" in \
      arm64) rustup target add aarch64-unknown-linux-musl ;; \
      *)     rustup target add x86_64-unknown-linux-musl ;; \
    esac

COPY src/ ./src/

RUN T="$(case "${TARGETARCH}" in arm64) echo aarch64-unknown-linux-musl ;; \
        *) echo x86_64-unknown-linux-musl ;; esac)" && \
    cargo build --release --target "${T}" && \
    mkdir -p /app/out && \
    install -m 0755 "target/${T}/release/youdao-llm-proxy" /app/out/youdao-llm-proxy

# ---------------------------------------------------------------- image ----
FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --create-home --uid 10001 youdao

COPY --from=builder /app/out/youdao-llm-proxy /usr/local/bin/youdao-llm-proxy

# Bind all interfaces so the port is reachable when published (containers
# otherwise keep the host default 127.0.0.1:8080).
ENV YOUDAO_BIND=0.0.0.0:8080
EXPOSE 8080

USER youdao
ENTRYPOINT ["/usr/local/bin/youdao-llm-proxy"]
