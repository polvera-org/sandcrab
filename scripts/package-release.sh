#!/usr/bin/env bash
# Build and package one Sandcrab release archive for a target triple (native builds only).
#
# Usage: scripts/package-release.sh <target-triple> [--out <dir>] [--expect-version <x.y.z>] [--dry-run]
#
# Targets (each built on a host of the same OS and arch; no cross-compiling):
#   aarch64-apple-darwin        sandcrab, crabagent, sandcrab-krun (ad-hoc signed), crabd (aarch64 musl)
#   x86_64-unknown-linux-gnu    sandcrab, crabagent, sandcrab-krun, crabd (x86_64 musl)
#   aarch64-unknown-linux-gnu   sandcrab, crabagent, sandcrab-krun, crabd (aarch64 musl)
#
# Output, in --out (default: <target dir>/dist):
#   sandcrab-<version>-<target>.tar.gz          one top-level dir sandcrab-<version>-<target>/
#   sandcrab-<version>-<target>.tar.gz.sha256   "<sha256>  <archive name>"
#
# Archive contents: the binaries above, crabd.sha256, LICENSE, README.md, INSTALL.md,
# templates/ (Dockerfiles + manifests + build.sh, so ./templates resolves from the extracted
# dir), scripts/fetch-libkrun.sh (`sandcrab libkrun install` looks for it in the cwd for
# from-source builds) and, on Linux, scripts/fc-net-setup.sh (Firecracker tap networking).
#
# --expect-version  fail unless the workspace version equals this (CI passes the tag minus `v`).
# --dry-run         print the plan (version, commands, archive layout) without building.
#
# Environment:
#   CARGO_TARGET_DIR        honoured (absolute, or relative to the directory you run this from).
#   SANDCRAB_SIGN_IDENTITY  passed through to scripts/sign-krun-helper.sh (default `-`, ad hoc).
#                           Developer ID signing + notarization are review item R10, not done here.
# Requires: rustup targets <target> and <arch>-unknown-linux-musl; on Linux musl-tools.
set -euo pipefail

usage() {
  sed -n '2,4p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 2
}

# Resolve relative paths against the caller's cwd (as cargo would) before cd.
abspath() {
  case "$1" in
    /*) printf '%s\n' "$1" ;;
    *) printf '%s/%s\n' "$(pwd)" "$1" ;;
  esac
}

target=""
out=""
expect_version=""
dry_run=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --out)
      [ "$#" -ge 2 ] || { echo "--out needs a value" >&2; exit 2; }
      out="$2"; shift ;;
    --out=*) out="${1#--out=}" ;;
    --expect-version)
      [ "$#" -ge 2 ] || { echo "--expect-version needs a value" >&2; exit 2; }
      expect_version="$2"; shift ;;
    --expect-version=*) expect_version="${1#--expect-version=}" ;;
    --dry-run) dry_run=1 ;;
    -h | --help) usage ;;
    -*) echo "unknown option: $1" >&2; usage ;;
    *)
      [ -z "$target" ] || { echo "unexpected argument: $1" >&2; usage; }
      target="$1" ;;
  esac
  shift
done
[ -n "$target" ] || usage

if [ -n "${CARGO_TARGET_DIR:-}" ]; then
  CARGO_TARGET_DIR="$(abspath "$CARGO_TARGET_DIR")"
  export CARGO_TARGET_DIR
fi
[ -z "$out" ] || out="$(abspath "$out")"

repo="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo"
target_dir="${CARGO_TARGET_DIR:-$repo/target}"
[ -n "$out" ] || out="$target_dir/dist"

case "$target" in
  aarch64-apple-darwin) os=macos; arch=aarch64 ;;
  x86_64-unknown-linux-gnu) os=linux; arch=x86_64 ;;
  aarch64-unknown-linux-gnu) os=linux; arch=aarch64 ;;
  *)
    echo "unsupported target: $target (aarch64-apple-darwin, x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu)" >&2
    exit 2 ;;
esac

# Native builds only: the host triple must match (sandcrab-krun signing and the musl crabd
# toolchain setup assume it).
host="$(rustc -vV | sed -n 's/^host: //p')"
if [ "$host" != "$target" ]; then
  if [ "$dry_run" = 1 ]; then
    echo "warning: host is $host, not $target (a real run refuses this)" >&2
  else
    echo "error: host is $host; build $target on a $target host (no cross builds)" >&2
    exit 1
  fi
fi

# Workspace version, as Cargo resolves it (sandcrab-cli inherits version.workspace).
version="$(cargo metadata --no-deps --format-version 1 --offline \
  | tr ',' '\n' | grep -A1 -x '{"name":"sandcrab-cli"' | sed -n 's/^"version":"\(.*\)"$/\1/p' | head -n1)"
if [ -z "$version" ]; then
  # Field order differs (or the package moved): ask cargo for the package id instead.
  version="$(cargo pkgid --offline -p sandcrab-cli | sed 's/.*[#@]//')"
fi
case "$version" in
  [0-9]*.[0-9]*.[0-9]*) ;;
  *) echo "error: could not determine the workspace version (got '$version')" >&2; exit 1 ;;
esac
if [ -n "$expect_version" ] && [ "$expect_version" != "$version" ]; then
  echo "error: workspace version $version != expected $expect_version (tag/Cargo.toml mismatch)" >&2
  exit 1
fi

name="sandcrab-${version}-${target}"
stage="$out/$name"
archive="$out/$name.tar.gz"
bin_dir="$target_dir/$target/release"
crabd_bin="$target_dir/${arch}-unknown-linux-musl/release/crabd"
bins="sandcrab crabagent sandcrab-krun"
extras="LICENSE README.md templates scripts/fetch-libkrun.sh"
[ "$os" = linux ] && extras="$extras scripts/fc-net-setup.sh"

build_cmd="cargo build --release --locked --target $target -p sandcrab-cli -p sandcrab-agent -p sandcrab-backend-libkrun --bin sandcrab --bin crabagent --bin sandcrab-krun"

if [ "$dry_run" = 1 ]; then
  cat <<EOF
version:  $version
target:   $target ($os, $arch)
host:     $host
build:    $build_cmd
crabd:    scripts/build-crabd.sh $arch  -> $crabd_bin
EOF
  if [ "$os" = macos ]; then
    echo "sign:     scripts/sign-krun-helper.sh --force $stage/sandcrab-krun (identity ${SANDCRAB_SIGN_IDENTITY:--})"
  fi
  echo "archive:  $archive (+ .sha256)"
  echo "layout:"
  for f in $bins crabd crabd.sha256 INSTALL.md $extras; do
    echo "  $name/$f"
  done
  exit 0
fi

# shellcheck disable=SC2086 # build_cmd is a fixed word list
$build_cmd
scripts/build-crabd.sh "$arch"

for f in $bins; do
  [ -x "$bin_dir/$f" ] || { echo "error: missing build output $bin_dir/$f" >&2; exit 1; }
done
[ -x "$crabd_bin" ] || { echo "error: missing $crabd_bin" >&2; exit 1; }

# Only ever remove our own staging dir and archive.
rm -rf "$stage" "$archive" "$archive.sha256"
mkdir -p "$stage/scripts"
for f in $bins; do
  cp "$bin_dir/$f" "$stage/$f"
  chmod 0755 "$stage/$f"
done
cp "$crabd_bin" "$stage/crabd"
chmod 0755 "$stage/crabd"
cp "$crabd_bin.sha256" "$stage/crabd.sha256" # "<sha256>  crabd", checkable in place
for f in $extras; do
  case "$f" in
    */*) cp "$f" "$stage/$f" ;;
    *) cp -R "$f" "$stage/$f" ;;
  esac
done

if [ "$os" = macos ]; then
  scripts/sign-krun-helper.sh --force "$stage/sandcrab-krun"
fi

if [ "$os" = macos ]; then
  # shellcheck disable=SC2016 # literal shell for INSTALL.md
  libkrun_install='brew install libkrun libkrunfw e2fsprogs   # pinned versions: see docs/hosting.md
export PATH="$(brew --prefix e2fsprogs)/sbin:$PATH"
sandcrab libkrun install --helper ./sandcrab-krun   # copies + re-signs the helper
sandcrab doctor --backend libkrun'
  # shellcheck disable=SC2016 # literal Markdown backticks
  quarantine='Downloaded with a browser? Clear the quarantine flag first (the binaries are ad-hoc signed,
not notarized yet): `xattr -dr com.apple.quarantine .`
'
else
  libkrun_install='sandcrab libkrun install --helper ./sandcrab-krun   # builds pinned libkrun via scripts/fetch-libkrun.sh
sandcrab doctor --backend libkrun

# Firecracker (KVM):
sandcrab doctor --backend firecracker --fix          # fetches the pinned firecracker + kernel
sudo scripts/fc-net-setup.sh                         # tap networking'
  quarantine=''
fi

cat >"$stage/INSTALL.md" <<EOF
# Sandcrab $version ($target)

Run these from this directory: \`./templates\` and \`scripts/fetch-libkrun.sh\` are found
relative to the current directory.

${quarantine}
\`\`\`sh
sha256sum -c crabd.sha256 || shasum -a 256 -c crabd.sha256
export PATH="\$PWD:\$PATH"
export SANDCRAB_CRABD="\$PWD/crabd"   # static musl guest agent (or pass --crabd)

# Docker backend:
templates/build.sh
sandcrab --backend docker run -t base -- 'uname -a'

# libkrun backend:
$libkrun_install
\`\`\`

Host setup for every backend: https://github.com/polvera-org/sandcrab/blob/v$version/docs/hosting.md
EOF

# COPYFILE_DISABLE: no AppleDouble (._*) entries from macOS tar.
(cd "$out" && COPYFILE_DISABLE=1 tar -czf "$name.tar.gz" "$name")
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$out" && sha256sum "$name.tar.gz" >"$name.tar.gz.sha256")
else
  (cd "$out" && shasum -a 256 "$name.tar.gz" >"$name.tar.gz.sha256")
fi
rm -rf "$stage"

echo "archive: $archive"
echo "sha256:  $(cut -d' ' -f1 "$archive.sha256")"
