#!/usr/bin/env bash
# Build (or reuse from cache) the microVM (Firecracker / libkrun) ext4 rootfs for a Sandcrab template.
#
#   scripts/build-rootfs.sh <template> --crabd <path> [--arch linux/amd64|linux/arm64] [--out <cache-root>] [--mkfs native|container|fakeroot] [--verify]
#
# --arch (alias --platform) defaults to the host arch; crabd must be built for it (scripts/build-crabd.sh).
#
# Thin wrapper over the `sandcrab-rootfs` binary (crates/sandcrab-rootfs); all logic
# lives in Rust (`rootfs::RootfsBuilder`). Output:
#   <cache-root>/rootfs/<cache-key>/{rootfs.ext4,manifest.json}   (default cache-root: ~/.sandcrab/cache)
# Prints the rootfs.ext4 path on stdout.
#
# Requires: docker, the template image (templates/build.sh <template>), and a static musl crabd.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
case "${1:-}" in
  ""|-h|--help) sed -n '2,13p' "$0"; exit 0 ;;
esac
template="$1"; shift

exec cargo run --quiet --release --manifest-path "$repo/Cargo.toml" \
  -p sandcrab-rootfs --bin sandcrab-rootfs -- \
  build "$template" --templates-dir "$repo/templates" "$@"
