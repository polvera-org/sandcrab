# syntax=docker/dockerfile:1
# sandcrab/render:0.1.0 — headless Chromium + Playwright + ffmpeg + ImageMagick + Typst.
#
# Pinned: node 26.13.1 (image digest), playwright 1.65.1 (npm, with its
# version-matched bundled Chromium in /opt/ms-playwright), Typst 0.15.1
# (release tarball, sha256-verified). ffmpeg/ImageMagick/fonts/Chromium
# system deps come from trixie apt (unpinned; see base for snapshot.debian.org note).
#
# Chromium choice: Playwright's bundled Chromium (installed with
# `playwright install --with-deps chromium`) rather than Debian's `chromium`
# package — it is the exact build Playwright 1.65.1 is tested against, so
# `chromium.launch()` works with no executablePath override (both the full
# Chromium and the headless shell Playwright uses for headless launches are
# installed). `chromium` on PATH is a wrapper around that same binary that adds
# --no-sandbox (Chromium's own sandbox cannot start as an arbitrary non-root
# uid under Docker's default seccomp; the sandbox itself is the boundary —
# Playwright already defaults to chromiumSandbox: false).
#
# Non-root: browsers in /opt/ms-playwright (world-readable), globals in
# /usr/local/lib/node_modules with NODE_PATH set so `require('playwright')`
# resolves from any cwd; caches under $HOME (/home/sandcrab, 1777).
FROM node:26.13.1-trixie-slim@sha256:e6bedef217677b276c591795e169840edd9096dbf13f29df8ef8d1856b8820bc AS node-runtime

FROM sandcrab/base:0.1.0 AS typst
ARG TARGETARCH
ARG TYPST_VERSION=0.15.1
ARG TYPST_SHA256_AMD64=a6d077d0a95eed5a2eba715b2dae06be954f624ccbf85758a03f389ded33118c
ARG TYPST_SHA256_ARM64=5aa8d74a3d906e60ea12a66ac2f37f8eef1b14cbad7182a745e393a10c23dcee
RUN set -eu; \
    case "${TARGETARCH:-amd64}" in \
      amd64) triple=x86_64-unknown-linux-musl; sum="${TYPST_SHA256_AMD64}" ;; \
      arm64) triple=aarch64-unknown-linux-musl; sum="${TYPST_SHA256_ARM64}" ;; \
      *) echo "unsupported arch ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/typst.tar.xz \
      "https://github.com/typst/typst/releases/download/v${TYPST_VERSION}/typst-${triple}.tar.xz"; \
    echo "${sum}  /tmp/typst.tar.xz" | sha256sum -c -; \
    tar -xJf /tmp/typst.tar.xz -C /tmp; \
    install -m 0755 "/tmp/typst-${triple}/typst" /usr/local/bin/typst

FROM sandcrab/base:0.1.0

ARG PLAYWRIGHT_VERSION=1.65.1

ENV NODE_PATH=/usr/local/lib/node_modules \
    PLAYWRIGHT_BROWSERS_PATH=/opt/ms-playwright \
    PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 \
    NPM_CONFIG_UPDATE_NOTIFIER=false \
    NPM_CONFIG_FUND=false

COPY --from=node-runtime /usr/local/bin/node /usr/local/bin/node
COPY --from=node-runtime /usr/local/lib/node_modules /usr/local/lib/node_modules
COPY --from=typst /usr/local/bin/typst /usr/local/bin/typst

RUN ln -sf ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -sf ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
    && apt-get update && apt-get install -y --no-install-recommends \
        libatomic1 \
        ffmpeg \
        imagemagick \
        fontconfig \
        fonts-dejavu-core \
        fonts-liberation \
        fonts-noto-core \
        fonts-noto-color-emoji \
    && npm install -g --cache /tmp/npm-build-cache "playwright@${PLAYWRIGHT_VERSION}" \
    && PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD= playwright install --with-deps chromium \
    && rm -rf /var/lib/apt/lists/* /tmp/npm-build-cache /root/.npm \
    && chmod -R a+rX /opt/ms-playwright /usr/local/lib/node_modules \
    && chrome="$(ls -d /opt/ms-playwright/chromium-*/chrome-linux*/chrome | head -n1)" \
    && test -x "$chrome" \
    && printf '#!/bin/sh\n# The sandbox (container/microVM) is the isolation boundary; the Chromium\n# setuid/userns sandbox is unavailable to an arbitrary non-root uid.\nexec %s --no-sandbox "$@"\n' "$chrome" > /usr/local/bin/chromium \
    && chmod 0755 /usr/local/bin/chromium \
    && fc-cache -f

WORKDIR /workspace
