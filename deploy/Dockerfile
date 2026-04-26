# syntax=docker/dockerfile:1
# Multi-stage build for the Claw CLI agent.

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

RUN cargo build --release -p rusty-claude-cli \
    && cp target/release/claw /usr/local/bin/claw

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

RUN groupadd -r claw \
    && useradd -r -g claw -d /workspace -s /bin/bash claw

COPY --from=builder /usr/local/bin/claw /usr/local/bin/claw
RUN chmod +x /usr/local/bin/claw

ENV CLAW_HOME=/workspace
ENV CLAW_PERMISSION=ask
WORKDIR /workspace
RUN mkdir -p /workspace && chown -R claw:claw /workspace
USER claw

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD claw --version || exit 1

ENTRYPOINT ["claw"]
CMD ["--help"]
