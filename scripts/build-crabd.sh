#!/usr/bin/env bash
# Build a fully static `crabd` (musl) for injection into sandbox containers / microVM rootfs.
#
# Usage: scripts/build-crabd.sh [x86_64|aarch64]   (default: host arch)
# Prints the binary path and its SHA-256 (also written next to it as crabd.sha256).
# Requires: rustup target <arch>-unknown-linux-musl, and musl-tools (musl-gcc) on Linux hosts.
set -euo pipefail

cd "$(dirname "$0")/.."

arch="${1:-$(uname -m)}"
case "$arch" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) echo "unsupported arch: $arch" >&2; exit 1 ;;
esac
target="${arch}-unknown-linux-musl"

if ! rustup target list --installed | grep -qx "$target"; then
  echo "missing Rust target $target: rustup target add $target" >&2
  exit 1
fi

# Static CRT for musl is the Rust default; set it explicitly so a user RUSTFLAGS cannot drop it.
export RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+crt-static"
cargo build --release --locked -p sandcrab-guest --bin crabd --target "$target" >&2

bin="target/${target}/release/crabd"
if command -v file >/dev/null 2>&1; then
  if ! file -L "$bin" | grep -qE 'static(-pie)? linked'; then
    echo "error: $bin is not statically linked: $(file -L "$bin")" >&2
    exit 1
  fi
fi

if command -v sha256sum >/dev/null 2>&1; then
  sha="$(sha256sum "$bin" | cut -d' ' -f1)"
else
  sha="$(shasum -a 256 "$bin" | cut -d' ' -f1)"
fi
echo "$sha  crabd" > "${bin}.sha256"
size="$(stat -c %s "$bin" 2>/dev/null || stat -f %z "$bin")"
echo "path:   $(pwd)/$bin"
echo "sha256: $sha"
echo "size:   $size bytes"
