FROM node:26-trixie-slim AS node-runtime
FROM oven/bun:latest AS bun-bin
FROM sandcrab/base:0.1.0

COPY --from=node-runtime /usr/local/bin/node /usr/local/bin/node
COPY --from=node-runtime /usr/local/lib/node_modules /usr/local/lib/node_modules
COPY --from=bun-bin /usr/local/bin/bun /usr/local/bin/bunx /usr/local/bin/

RUN ln -sf /usr/local/lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -sf /usr/local/lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
    && ln -sf /usr/local/lib/node_modules/corepack/dist/corepack.js /usr/local/bin/corepack \
    && corepack enable \
    && corepack prepare pnpm@latest --activate \
    && npm install -g typescript tsx vitest @biomejs/biome

WORKDIR /workspace
