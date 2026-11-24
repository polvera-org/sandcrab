#!/usr/bin/env bash
# Sign the `sandcrab-krun` helper with the Hypervisor.framework entitlement (macOS only).
#
# Usage: scripts/sign-krun-helper.sh [--force] <helper> [<helper>...]
#
# Only the helper creates VMs, so only it needs `com.apple.security.hypervisor`
# (crates/sandcrab-backend-libkrun/entitlements.plist). The SDK, CLI and test binaries need no
# entitlement. Design: docs/design/libkrun-backend.md §4.
#
# Environment:
#   SANDCRAB_SIGN_IDENTITY      codesign identity. Default `-` (ad-hoc, for dev). A Developer ID
#                               ("Developer ID Application: …") also gets the hardened runtime
#                               and a secure timestamp (`--options runtime --timestamp`).
#                               Notarization is a separate release step, not done here.
#   SANDCRAB_KRUN_ENTITLEMENTS  entitlements plist (default: the one in the libkrun crate).
#
# Idempotent: with the ad-hoc identity, a binary that already carries the entitlement is left
# alone (pass --force to re-sign). A Developer ID always re-signs. On non-macOS hosts this is a
# no-op (Linux KVM needs no signature).
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
identity="${SANDCRAB_SIGN_IDENTITY:--}"
entitlements="${SANDCRAB_KRUN_ENTITLEMENTS:-$repo/crates/sandcrab-backend-libkrun/entitlements.plist}"
key="com.apple.security.hypervisor"

force=0
if [ "${1:-}" = "--force" ]; then
  force=1
  shift
fi
if [ "$#" -eq 0 ]; then
  echo "usage: $0 [--force] <helper> [<helper>...]" >&2
  exit 2
fi

if [ "$(uname -s)" != "Darwin" ]; then
  echo "sign-krun-helper: not macOS, nothing to sign"
  exit 0
fi

if [ ! -f "$entitlements" ]; then
  echo "sign-krun-helper: entitlements not found: $entitlements" >&2
  exit 1
fi

has_entitlement() {
  # `codesign -d --entitlements -` prints the plist (or its key/value dump) on stdout.
  codesign -d --entitlements - "$1" 2>/dev/null | grep -q "$key"
}

for bin in "$@"; do
  if [ ! -f "$bin" ]; then
    echo "sign-krun-helper: no such file: $bin" >&2
    exit 1
  fi

  if [ "$identity" = "-" ] && [ "$force" -eq 0 ] && has_entitlement "$bin"; then
    echo "sign-krun-helper: $bin already has $key (ad-hoc), skipping"
    continue
  fi

  args=(--force --sign "$identity" --entitlements "$entitlements")
  if [ "$identity" != "-" ]; then
    # Developer ID: hardened runtime + secure timestamp (required for notarization).
    args+=(--options runtime --timestamp)
  fi
  codesign "${args[@]}" "$bin"

  codesign --verify --strict "$bin"
  if ! has_entitlement "$bin"; then
    echo "sign-krun-helper: $bin is signed but lacks $key" >&2
    exit 1
  fi
  if [ "$identity" = "-" ]; then
    echo "sign-krun-helper: signed $bin (ad-hoc) with $key"
  else
    echo "sign-krun-helper: signed $bin ($identity, hardened runtime) with $key"
  fi
done
