# syntax=docker/dockerfile:1
# sandcrab/base:0.1.0 — minimal Debian 13 (trixie) sandbox root.
#
# Pinning: base image is pinned by dated tag + multi-arch index digest.
# apt packages are NOT pinned yet; future work: build against
# snapshot.debian.org (e.g. deb http://snapshot.debian.org/archive/debian/20261104T000000Z trixie main)
# for byte-reproducible package sets.
#
# Runtime contract (see templates/README note in build.sh):
#   - no crabd baked in: backends bind-mount it read-only at /opt/sandcrab/crabd
#     (DockerDev) or inject it into the ext4 rootfs (Firecracker).
#   - /workspace (WORKDIR) and /run/sandcrab exist.
#   - runs as an arbitrary non-root uid:gid. HOME=/home/sandcrab is a sticky,
#     world-writable (1777) dir so any uid can create caches/config there
#     (not under /tmp, so it survives a tmpfs /tmp). A `sandcrab` user (1000:1000)
#     exists so uid 1000 has a passwd entry (whoami, git, ssh).
#   - every tool lands in /usr/local/bin, because `bash -l` (/etc/profile)
#     resets PATH and would drop extra dirs set via ENV.
#   - no ENTRYPOINT/CMD: the backend supplies the command.
FROM debian:trixie-20261104-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2

ENV DEBIAN_FRONTEND=noninteractive \
    LANG=C.UTF-8 \
    LC_ALL=C.UTF-8 \
    HOME=/home/sandcrab \
    XDG_CACHE_HOME=/home/sandcrab/.cache \
    XDG_CONFIG_HOME=/home/sandcrab/.config \
    XDG_DATA_HOME=/home/sandcrab/.local/share

RUN apt-get update && apt-get install -y --no-install-recommends \
        bash \
        ca-certificates \
        curl \
        fd-find \
        git \
        jq \
        less \
        procps \
        ripgrep \
        tar \
        unzip \
        xz-utils \
        zstd \
    && rm -rf /var/lib/apt/lists/* \
    && ln -sf /usr/bin/fdfind /usr/local/bin/fd \
    && groupadd --gid 1000 sandcrab \
    && useradd --uid 1000 --gid 1000 --home-dir /home/sandcrab --no-create-home --shell /bin/bash sandcrab \
    && mkdir -p /workspace /run/sandcrab /opt/sandcrab /home/sandcrab \
    && chmod 1777 /home/sandcrab /workspace \
    && chmod 0755 /run/sandcrab /opt/sandcrab \
    # Workspaces are often owned by a different uid than the sandbox user.
    && git config --system safe.directory '*' \
    && git config --system init.defaultBranch main

WORKDIR /workspace
