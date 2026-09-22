# syntax=docker/dockerfile:1.7

# Build natively for each target platform under buildx: the builder stage
# runs on linux/amd64 natively and on linux/arm64 via QEMU, so the C parts
# of ring always compile with the platform's own gcc - no cross C toolchain
# (and no host-glibc headers) are required.
FROM rust:1 AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
RUN cargo fetch

COPY src/ ./src/

RUN cargo build --release && \
    install -m 0755 target/release/youdao-llm-proxy /usr/local/bin/youdao-llm-proxy

# ---------------------------------------------------------------- image ----
FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --create-home --uid 10001 youdao

COPY --from=builder /usr/local/bin/youdao-llm-proxy /usr/local/bin/youdao-llm-proxy

# Bind all interfaces so the port is reachable when published (containers
# otherwise keep the host default 127.0.0.1:8080).
ENV YOUDAO_BIND=0.0.0.0:8080
EXPOSE 8080

USER youdao
ENTRYPOINT ["/usr/local/bin/youdao-llm-proxy"]
