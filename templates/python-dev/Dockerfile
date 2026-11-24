# syntax=docker/dockerfile:1
# sandcrab/python-dev:0.1.0 — Python 3.15 (uv-managed) + ruff/pytest/mypy.
#
# Pinned: uv 0.13.9 (image digest), CPython 3.15.2, ruff 0.17.6,
# pytest 9.2.0, mypy 2.4.1. Target Python 3.15 is available — no fallback.
#
# Non-root: interpreter + tool venvs live under /opt/uv (world-readable),
# entry points are linked into /usr/local/bin. Agents install deps into a
# project venv (`uv venv && uv pip install ...` / `uv sync`); the uv cache
# goes to $HOME/.cache/uv (HOME=/home/sandcrab, 1777, from base).
#
# Multi-arch (linux/amd64, linux/arm64): the uv digest is an OCI index digest,
# so `COPY --from=uv-bin` takes the target arch's binary. CPython comes from
# `uv python install`, which picks the target arch's python-build-standalone
# build and verifies it against the sha256 table embedded in the pinned uv.
# BASE_IMAGE is set by templates/build.sh to the arch-suffixed base tag.
ARG BASE_IMAGE=sandcrab/base:0.1.0
FROM ghcr.io/astral-sh/uv:0.13.9@sha256:d3b601f0c7b118bc1753febf7c41cc80981df678445e0254284eecf84db79a7d AS uv-bin

FROM ${BASE_IMAGE}

ARG PYTHON_VERSION=3.15.2
ARG RUFF_VERSION=0.17.6
ARG PYTEST_VERSION=9.2.0
ARG MYPY_VERSION=2.4.1

ENV UV_PYTHON_INSTALL_DIR=/opt/uv/python \
    UV_PYTHON_BIN_DIR=/usr/local/bin \
    UV_TOOL_DIR=/opt/uv/tools \
    UV_TOOL_BIN_DIR=/usr/local/bin \
    UV_PYTHON_DOWNLOADS=manual \
    UV_LINK_MODE=copy \
    PYTHONDONTWRITEBYTECODE=1

COPY --from=uv-bin /uv /uvx /usr/local/bin/

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        pkg-config \
    && rm -rf /var/lib/apt/lists/* \
    && UV_CACHE_DIR=/tmp/uv-build-cache uv python install "${PYTHON_VERSION}" --default --preview-features python-install-default \
    && UV_CACHE_DIR=/tmp/uv-build-cache uv tool install --python "${PYTHON_VERSION}" "ruff==${RUFF_VERSION}" \
    && UV_CACHE_DIR=/tmp/uv-build-cache uv tool install --python "${PYTHON_VERSION}" "pytest==${PYTEST_VERSION}" \
    && UV_CACHE_DIR=/tmp/uv-build-cache uv tool install --python "${PYTHON_VERSION}" "mypy==${MYPY_VERSION}" \
    && rm -rf /tmp/uv-build-cache \
    && chmod -R a+rX /opt/uv

WORKDIR /workspace
