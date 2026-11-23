FROM ghcr.io/astral-sh/uv:latest AS uv-bin
FROM sandcrab/base:0.1.0

ENV DEBIAN_FRONTEND=noninteractive \
    UV_PYTHON_INSTALL_DIR=/opt/python \
    PATH="/opt/python/bin:/root/.local/bin:${PATH}"

COPY --from=uv-bin /uv /uvx /usr/local/bin/

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    pkg-config \
    && rm -rf /var/lib/apt/lists/* \
    && uv python install 3.15 --default \
    && uv tool install ruff \
    && uv tool install pytest \
    && uv tool install mypy

WORKDIR /workspace
