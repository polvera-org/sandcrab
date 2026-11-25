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
#   SANDCRAB_SIGN_RUNTIME=1     hardened runtime even ad hoc (tests the Developer ID setup).
#   SANDCRAB_KRUN_ENTITLEMENTS  entitlements plist. Default: entitlements.plist (hypervisor
#                               only), or entitlements-hardened.plist (hypervisor +
#                               com.apple.security.cs.disable-library-validation) with the
#                               hardened runtime, which otherwise refuses to load libkrun
#                               (another team's or an ad-hoc signature; review R10). The helper
#                               verifies libkrun/libkrunfw itself (pinned SHA-256, fd-loaded).
#
# After signing, `sandcrab libkrun install --helper <bin>` writes the helper's install record;
# a helper signed in place by this script needs that re-run (its ctime and digest changed).
#
# Idempotent: with the ad-hoc identity, a binary that already carries the entitlement is left
# alone (pass --force to re-sign). A Developer ID always re-signs. On non-macOS hosts this is a
# no-op (Linux KVM needs no signature).
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
identity="${SANDCRAB_SIGN_IDENTITY:--}"
runtime=0
if [ "$identity" != "-" ] || [ "${SANDCRAB_SIGN_RUNTIME:-}" = 1 ]; then
  runtime=1
fi
if [ "$runtime" = 1 ]; then
  default_entitlements="$repo/crates/sandcrab-backend-libkrun/entitlements-hardened.plist"
else
  default_entitlements="$repo/crates/sandcrab-backend-libkrun/entitlements.plist"
fi
entitlements="${SANDCRAB_KRUN_ENTITLEMENTS:-$default_entitlements}"
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

  if [ "$identity" = "-" ] && [ "$runtime" = 0 ] && [ "$force" -eq 0 ] && has_entitlement "$bin"; then
    echo "sign-krun-helper: $bin already has $key (ad-hoc), skipping"
    continue
  fi

  args=(--force --sign "$identity" --entitlements "$entitlements")
  if [ "$runtime" = 1 ]; then
    args+=(--options runtime)
  fi
  if [ "$identity" != "-" ]; then
    # Developer ID: secure timestamp (required for notarization).
    args+=(--timestamp)
  fi
  codesign "${args[@]}" "$bin"

  codesign --verify --strict "$bin"
  if ! has_entitlement "$bin"; then
    echo "sign-krun-helper: $bin is signed but lacks $key" >&2
    exit 1
  fi
  if [ "$runtime" = 1 ]; then
    echo "sign-krun-helper: signed $bin ($identity, hardened runtime) with $key + disable-library-validation"
  else
    echo "sign-krun-helper: signed $bin ($identity) with $key"
  fi
done
