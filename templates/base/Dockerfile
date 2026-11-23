FROM debian:trixie-slim

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
    bash \
    ca-certificates \
    curl \
    fd-find \
    git \
    jq \
    procps \
    ripgrep \
    tar \
    zstd \
    && rm -rf /var/lib/apt/lists/* \
    && ln -sf /usr/bin/fdfind /usr/local/bin/fd \
    && mkdir -p /workspace /run/sandcrab

# Phase 1B/1D: `crabd` static musl binary will be injected at /usr/local/bin/crabd
WORKDIR /workspace
