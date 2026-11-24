#!/usr/bin/env bash
# Download and SHA-256-verify the pinned guest kernel and Firecracker release binaries.
#
#   scripts/fetch-artifacts.sh [--out <cache-root>] [--no-jailer]
#
# Pins live in crates/sandcrab-backend-firecracker/src/artifacts.rs (mirrored in
# docs/design/firecracker-backend.md). Output: <cache-root>/{kernel,firecracker}/<version>/.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
case "${1:-}" in -h|--help) sed -n '2,8p' "$0"; exit 0 ;; esac

exec cargo run --quiet --release --manifest-path "$repo/Cargo.toml" \
  -p sandcrab-backend-firecracker --bin sandcrab-fc-fetch -- "$@"
