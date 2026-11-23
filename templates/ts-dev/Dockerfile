# syntax=docker/dockerfile:1
# sandcrab/ts-dev:0.1.0 — Node.js 26 (Active LTS) + npm + pnpm + bun + TS tooling.
#
# Pinned: node 26.13.1 / npm 11.20.0 (image digest), bun 1.4.5 (image digest),
# pnpm 12.21.0, typescript 7.0.2, tsx 4.23.15, vitest 5.1.1, @biomejs/biome 2.6.8.
# node needs libatomic1 (not in trixie-slim). python3 (Debian's, full stdlib, no pip/venv) is there
# because agents reach for it by reflex for one-off scripts; for Python projects use the
# python-dev template.
# npm 11.20 skips install scripts by default; pnpm and esbuild (via tsx/vitest)
# need theirs, so they are explicitly allowed.
# Node 26 no longer ships corepack, so pnpm is a pinned npm global.
#
# Non-root: globals live in /usr/local/lib/node_modules (world-readable) with
# bins in /usr/local/bin. Agents install project deps locally in /workspace;
# npm/pnpm/bun caches go under $HOME (HOME=/home/sandcrab, 1777, from base).
#
# No browser: Chromium + Playwright (and their system libraries) live in the `render`
# template; baking them in here would add ~400 MB. `npx playwright install` in ts-dev
# also lacks the shared libraries Chromium needs.
#
# $HOME is reset every session. To keep caches across sessions, point them into the
# persistent workspace: crabagent does this automatically when a read-write workspace is
# mounted at the cwd (XDG_CACHE_HOME=<cwd>/.cache, npm_config_cache=<cwd>/.cache/npm;
# Playwright and pip/uv follow XDG_CACHE_HOME). SDK/CLI users can pass the same env.
# (sandcrab.toml has no [env] table, so the manifest cannot set this conditionally.)
#
# /tmp is a noexec tmpfs under Docker: installers that unpack and run binaries from
# a temp dir need TMPDIR elsewhere, e.g. `mkdir -p ~/.tmp && export TMPDIR=~/.tmp`.
FROM node:26.13.1-trixie-slim@sha256:e6bedef217677b276c591795e169840edd9096dbf13f29df8ef8d1856b8820bc AS node-runtime
FROM oven/bun:1.4.5-slim@sha256:b780593f3e10918cbdaec3d5a08d52224b9198069a65eb47812085c2ec007643 AS bun-bin

FROM sandcrab/base:0.1.0

ARG PNPM_VERSION=12.21.0
ARG TYPESCRIPT_VERSION=7.0.2
ARG TSX_VERSION=4.23.15
ARG VITEST_VERSION=5.1.1
ARG BIOME_VERSION=2.6.8

ENV NODE_PATH=/usr/local/lib/node_modules \
    NPM_CONFIG_UPDATE_NOTIFIER=false \
    NPM_CONFIG_FUND=false

COPY --from=node-runtime /usr/local/bin/node /usr/local/bin/node
COPY --from=node-runtime /usr/local/lib/node_modules /usr/local/lib/node_modules
COPY --from=node-runtime /usr/local/include/node /usr/local/include/node
COPY --from=bun-bin /usr/local/bin/bun /usr/local/bin/bun

RUN apt-get update && apt-get install -y --no-install-recommends libatomic1 python3 \
    && rm -rf /var/lib/apt/lists/* \
    && ln -sf ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -sf ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
    && ln -sf bun /usr/local/bin/bunx \
    && npm install -g --cache /tmp/npm-build-cache --allow-scripts=pnpm,esbuild \
        "pnpm@${PNPM_VERSION}" \
        "typescript@${TYPESCRIPT_VERSION}" \
        "tsx@${TSX_VERSION}" \
        "vitest@${VITEST_VERSION}" \
        "@biomejs/biome@${BIOME_VERSION}" \
    && rm -rf /tmp/npm-build-cache /root/.npm \
    && chmod -R a+rX /usr/local/lib/node_modules

WORKDIR /workspace
