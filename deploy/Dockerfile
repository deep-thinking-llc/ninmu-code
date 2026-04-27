# syntax=docker/dockerfile:1
# Multi-stage build for the Ninmu Code CLI agent.

FROM rust:bookworm AS builder

WORKDIR /workspace
COPY rust/ .

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        git \
        libssl-dev \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

RUN cargo build --release -p ninmu-cli \
    && cp target/release/ninmu /usr/local/bin/ninmu

# ---------------------------------------------------------------------------
# Runtime stage — minimal debian image
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        git \
        curl \
        openssh-client \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd -r ninmu \
    && useradd -r -g ninmu -d /workspace -s /bin/bash ninmu

COPY --from=builder /usr/local/bin/ninmu /usr/local/bin/ninmu
RUN chmod +x /usr/local/bin/ninmu

ENV NINMU_HOME=/workspace
ENV NINMU_PERMISSION=ask
WORKDIR /workspace
RUN mkdir -p /workspace && chown -R ninmu:ninmu /workspace
USER ninmu

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ninmu --version || exit 1

ENTRYPOINT ["ninmu"]
CMD ["--help"]
