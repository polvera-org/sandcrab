#!/usr/bin/env bash
# Build (or reuse from cache) the Firecracker ext4 rootfs for a Sandcrab template.
#
#   scripts/build-rootfs.sh <template> --crabd <path> [--out <cache-root>] [--mkfs native|container|fakeroot] [--verify]
#
# Thin wrapper over the `sandcrab-rootfs` binary (crates/sandcrab-backend-firecracker); all logic
# lives in Rust (`rootfs::RootfsBuilder`). Output:
#   <cache-root>/rootfs/<cache-key>/{rootfs.ext4,manifest.json}   (default cache-root: ~/.sandcrab/cache)
# Prints the rootfs.ext4 path on stdout.
#
# Requires: docker, the template image (templates/build.sh <template>), and a static musl crabd.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
case "${1:-}" in
  ""|-h|--help) sed -n '2,12p' "$0"; exit 0 ;;
esac
template="$1"; shift

exec cargo run --quiet --release --manifest-path "$repo/Cargo.toml" \
  -p sandcrab-backend-firecracker --bin sandcrab-rootfs -- \
  build "$template" --templates-dir "$repo/templates" "$@"
