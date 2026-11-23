FROM sandcrab/base:0.1.0

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
    chromium \
    ffmpeg \
    fonts-dejavu-core \
    fonts-liberation \
    fonts-noto-color-emoji \
    imagemagick \
    && rm -rf /var/lib/apt/lists/*

# Phase 1E/2: add Playwright driver and pinned Typst release binary
ENV CHROMIUM_PATH=/usr/bin/chromium

WORKDIR /workspace
