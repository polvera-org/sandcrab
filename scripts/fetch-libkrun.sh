#!/usr/bin/env bash
# Install the pinned libkrun + libkrunfw into <SANDCRAB_HOME>/artifacts/libkrun/<version>/.
#
#   scripts/fetch-libkrun.sh [--from-source] [--fw-from-source] [--home <dir>]
#                            [--workdir <dir>] [--keep-workdir] [--dry-run]
#                            [--no-netstack | --netstack-only]
#
# macOS arm64, Homebrew present (default): copy the `libkrun`/`libkrunfw` bottle dylibs, verify
#   their pinned SHA-256s, rewrite the libkrunfw copy's install id to its leaf name and re-sign
#   it ad hoc (libkrun dlopens libkrunfw by leaf name; the helper pre-loads the pinned copy).
# --from-source: build both from the pinned git tags (commit-checked). The only option without
#   Homebrew. macOS: libkrunfw compiles the pinned prebuilt kernel bundle (no kernel build);
#   needs Rust, clang, lld and a Linux sysroot download for libkrun's init blob.
# Linux x86_64: libkrun always from source (no upstream binaries); libkrunfw from the pinned
#   prebuilt release asset, or with --fw-from-source a full kernel build. Build prereqs: Rust
#   (cargo), make, gcc, git, curl, patchelf; for --fw-from-source also bc, bison, flex,
#   libelf-dev, libssl-dev, python3-pyelftools, cpio, rsync.
# Netstack (network `unrestricted`, design §9 Phase B), into
#   <SANDCRAB_HOME>/artifacts/libkrun-net/<kind>-<version>/ with its own manifest.json:
#   macOS: the pinned gvproxy release asset (SHA-256 checked, installed byte for byte);
#   Linux: passt built statically (`make static`) from the pinned tag (commit-checked; needs
#   gcc, make and the static glibc from libc6-dev). --no-netstack skips it, --netstack-only
#   installs only it.
# --dry-run prints every mutating/build step instead of running it.
#
# Pins mirror crates/sandcrab-backend-libkrun/src/artifacts.rs and netstack_pins.rs (unit
# tests keep them in sync).
# Writes manifest.json last; `Artifacts::resolve` checks it against the compiled-in pins.
set -euo pipefail

LIBKRUN_VERSION="1.19.8"
LIBKRUNFW_VERSION="5.6.4"
LIBKRUN_REPO="https://github.com/libkrun/libkrun"
LIBKRUN_TAG="v1.19.8"
LIBKRUN_COMMIT="c4d283c233bc0ae1993c289ec2a1b4da2242f1f8"
LIBKRUNFW_REPO="https://github.com/libkrun/libkrunfw"
LIBKRUNFW_TAG="v5.6.4"
LIBKRUNFW_COMMIT="b31ee3033103aaabdfef63e3d12a6ada873ace92"
MANIFEST_SCHEMA="1"
# macOS arm64 (Homebrew bottles, arm64_tahoe)
MAC_LIBKRUN_BOTTLE_FILE="lib/libkrun.1.19.8.dylib"
MAC_LIBKRUN_BOTTLE_SHA256="3c2ffda8f326e6cdcbb504d8e7dc450f84dca8c57b001fce7b6bc6dbdaca5b5a"
MAC_LIBKRUNFW_BOTTLE_FILE="lib/libkrunfw.5.dylib"
MAC_LIBKRUNFW_BOTTLE_SHA256="1ccb5283921f36fedaa8f85ba6265e298a6b86d56be49f642c768b32102b91df"
MAC_LIBKRUN_FILE="libkrun.1.dylib"
MAC_LIBKRUNFW_FILE="libkrunfw.5.dylib"
MAC_FW_KERNEL_URL="https://github.com/libkrun/libkrunfw/releases/download/v5.6.4/libkrunfw-prebuilt-aarch64.tgz"
MAC_FW_KERNEL_SHA256="db126ed9775e0f92fc910a90a8b7ff52bdf55644b5edc75977115f81603e48c9"
# Linux x86_64
LINUX_LIBKRUN_FILE="libkrun.so.1"
LINUX_LIBKRUNFW_FILE="libkrunfw.so.5"
LINUX_FW_URL="https://github.com/libkrun/libkrunfw/releases/download/v5.6.4/libkrunfw-x86_64.tgz"
LINUX_FW_SHA256="25a5554137dcc11dd71246627264f67539154ee783aa8450acfa79139e802e64"
LINUX_FW_MEMBER="lib/x86_64-linux-gnu/libkrunfw.so.5.6.4"
LINUX_FW_MEMBER_SHA256="5ec9c03e97e2f6df7817b702eb8bfbc090b36c14186b33ddef4a2e9bfccb1c24"
# libkrunfw kernel source (its Makefile would curl this unverified; we pre-seed it)
FW_KERNEL_SRC_URL="https://github.com/libkrun/linux/archive/refs/tags/v5.6.4-6.12.115.tar.gz"
FW_KERNEL_SRC_SHA256="050fcea96b607e49f135f270472bc95346c172c9d6df5f9256aeb6f7972675d5"
# Userspace netstacks (netstack_pins.rs)
NETSTACK_MANIFEST_SCHEMA="1"
GVPROXY_VERSION="0.9.0"
GVPROXY_COMMIT="6dbb5d4fa0033356c72a201346f8350594229868"
GVPROXY_URL="https://github.com/containers/gvisor-tap-vsock/releases/download/v0.9.0/gvproxy-darwin"
GVPROXY_SHA256="e2a6825ba789fe588828b418c09aaf16525dec6d8cb2d16c94e72535dd4606be"
PASST_VERSION="2026_10_02.cba3570"
PASST_REPO="https://passt.top/passt"
PASST_TAG="2026_10_02.cba3570"
PASST_COMMIT="cba357068dc586e7540971eb40bb56c5e62c4de1"

die() { echo "fetch-libkrun: error: $*" >&2; exit 1; }
say() { echo "fetch-libkrun: $*" >&2; }

from_source=0 fw_from_source=0 dry=0 keep=0 home="" workdir="" netstack=1 netstack_only=0
while [ $# -gt 0 ]; do
  case "$1" in
    --from-source) from_source=1 ;;
    --fw-from-source) fw_from_source=1 ;;
    --home) home="${2:?--home needs a dir}"; shift ;;
    --workdir) workdir="${2:?--workdir needs a dir}"; shift ;;
    --keep-workdir) keep=1 ;;
    --dry-run) dry=1 ;;
    --no-netstack) netstack=0 ;;
    --netstack-only) netstack_only=1 ;;
    -h|--help) sed -n '2,28p' "$0"; exit 0 ;;
    *) die "unknown argument: $1 (see --help)" ;;
  esac
  shift
done

# Run a mutating/build step (printed instead under --dry-run).
run() {
  if [ "$dry" = 1 ]; then printf '+ %q' "$1" >&2; shift; printf ' %q' "$@" >&2; echo >&2; else "$@"; fi
}

sha256() {
  if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

verify() { # <file> <expected-sha256>
  local got
  if [ "$dry" = 1 ] && [ ! -e "$1" ]; then say "(dry-run) would verify $1 = $2"; return; fi
  got="$(sha256 "$1")"
  [ "$got" = "$2" ] || die "SHA-256 mismatch for $1: expected $2, got $got"
}

download() { # <url> <dest> <sha256>
  run curl -fsSL --proto '=https' --tlsv1.2 --retry 3 -o "$2" "$1"
  verify "$2" "$3"
}

clone_pinned() { # <repo> <tag> <commit> <dest>
  run git -c advice.detachedHead=false clone --quiet --depth 1 --branch "$2" "$1" "$4"
  if [ "$dry" = 1 ]; then say "(dry-run) would check $4 HEAD = $3"; return; fi
  local head; head="$(git -C "$4" rev-parse HEAD)"
  [ "$head" = "$3" ] || die "$1 tag $2 resolves to $head, pinned $3"
}

need() { for c in "$@"; do command -v "$c" >/dev/null || die "missing prerequisite: $c"; done; }

# Symbols the helper resolves at load time (crates/sandcrab-backend-libkrun/src/ffi.rs
# `Krun::load`): fail the install, not the first spawn, when a build lacks a feature.
REQUIRED_SYMBOLS="krun_set_log_level krun_create_ctx krun_free_ctx krun_set_vm_config
  krun_add_disk krun_set_root_disk_remount krun_add_virtiofs4 krun_disable_implicit_vsock
  krun_add_vsock krun_add_vsock_port2 krun_set_port_map krun_set_workdir krun_set_exec
  krun_get_max_vcpus krun_start_enter krun_add_net_unixstream"

check_symbols() { # <library>
  if [ "$dry" = 1 ]; then say "(dry-run) would check the exported symbols of $1"; return; fi
  need nm
  local syms s
  if [ "$plat" = macos ]; then syms="$(nm -gU "$1" | awk '{print $3}' | sed 's/^_//')"
  else syms="$(nm -D --defined-only "$1" | awk '{print $3}')"; fi
  for s in $REQUIRED_SYMBOLS; do
    printf '%s\n' "$syms" | grep -qx "$s" || die "$1 lacks $s (built without a needed feature?)"
  done
}

# ---- platform
os="$(uname -s)" arch="$(uname -m)"
case "$os/$arch" in
  Darwin/arm64) plat=macos; rarch=aarch64; krun_file="$MAC_LIBKRUN_FILE"; fw_file="$MAC_LIBKRUNFW_FILE" ;;
  Linux/x86_64) plat=linux; rarch=x86_64; krun_file="$LINUX_LIBKRUN_FILE"; fw_file="$LINUX_LIBKRUNFW_FILE" ;;
  *) die "no pinned libkrun for $os/$arch (supported: macOS arm64, Linux x86_64)" ;;
esac
[ "$netstack" = 0 ] && [ "$netstack_only" = 1 ] && die "--no-netstack and --netstack-only conflict"
if [ -z "$home" ]; then home="${SANDCRAB_HOME:-${HOME:?HOME unset}/.sandcrab}"; fi

# ---- userspace netstack (gvproxy on macOS, passt on Linux)
install_netstack() {
  local kind version file origin osha commit nparent ndest nstage src
  if [ "$plat" = macos ]; then
    kind=gvproxy version="$GVPROXY_VERSION" origin=release osha="\"$GVPROXY_SHA256\"" commit=null
  else
    kind=passt version="$PASST_VERSION" origin=source osha=null commit="\"$PASST_COMMIT\""
  fi
  file="$kind"
  nparent="$home/artifacts/libkrun-net"
  ndest="$nparent/$kind-$version"
  nstage="$nparent/.$kind-$version.staging.$$"
  say "netstack: $kind $version → $ndest"
  run mkdir -p "$workdir"
  if [ "$plat" = macos ]; then
    download "$GVPROXY_URL" "$workdir/gvproxy" "$GVPROXY_SHA256"
    src="$workdir/gvproxy"
  else
    need git make gcc
    clone_pinned "$PASST_REPO" "$PASST_TAG" "$PASST_COMMIT" "$workdir/passt"
    # Static: no runtime dependency on the host's glibc; no seccomp/AppArmor profile is
    # installed (passt sandboxes itself: namespaces, seccomp, dropped capabilities).
    run make -C "$workdir/passt" -j"$( (nproc || echo 4) 2>/dev/null)" static
    src="$workdir/passt/passt"
  fi
  if [ "$dry" = 1 ]; then say "(dry-run) would install $src into $ndest"; return; fi
  rm -rf "$nstage"
  mkdir -p "$nstage"
  cp "$src" "$nstage/$file"
  chmod 0555 "$nstage/$file"
  [ "$origin" != release ] || [ "\"$(sha256 "$nstage/$file")\"" = "$osha" ] || die "$kind copy differs from its verified download"
  printf '{\n  "schema": %s,\n  "os": "%s",\n  "arch": "%s",\n  "kind": "%s",\n  "version": "%s",\n  "file": "%s",\n  "origin": "%s",\n  "origin_sha256": %s,\n  "git_commit": %s,\n  "sha256": "%s"\n}\n' \
    "$NETSTACK_MANIFEST_SCHEMA" "$plat" "$rarch" "$kind" "$version" "$file" "$origin" "$osha" "$commit" \
    "$(sha256 "$nstage/$file")" > "$nstage/manifest.json"
  local nold="$nparent/.$kind-$version.old.$$"
  [ -e "$ndest" ] && mv "$ndest" "$nold"
  mv "$nstage" "$ndest"
  rm -rf "$nold"
  say "netstack installed:"
  cat "$ndest/manifest.json" >&2
}

if [ "$netstack_only" = 1 ]; then
  if [ -z "$workdir" ]; then workdir="$(mktemp -d "${TMPDIR:-/tmp}/sandcrab-netstack.XXXXXX")"; fi
  trap '[ "$keep" = 1 ] || rm -rf "$workdir"' EXIT
  install_netstack
  exit 0
fi

if [ "$plat" = macos ] && [ "$from_source" = 0 ]; then
  if ! command -v brew >/dev/null; then
    die "Homebrew not found: building from source is the only option on this host.
  Re-run with --from-source (needs Rust, clang, lld; downloads a Linux sysroot)."
  fi
  mode=homebrew
elif [ "$plat" = macos ]; then
  mode=source
else
  [ "$from_source" = 1 ] || say "Linux: libkrun is built from source (no upstream binaries)"
  mode=source
fi
[ "$fw_from_source" = 1 ] && [ "$plat" != linux ] && die "--fw-from-source is Linux-only (macOS uses the pinned prebuilt kernel bundle)"

parent="$home/artifacts/libkrun"
dest="$parent/$LIBKRUN_VERSION"
say "$plat/$rarch, mode=$mode$([ "$fw_from_source" = 1 ] && echo ' (libkrunfw from source)'), dest=$dest"

# ---- scratch
if [ -z "$workdir" ]; then workdir="$(mktemp -d "${TMPDIR:-/tmp}/sandcrab-libkrun.XXXXXX")"; else mkdir -p "$workdir"; fi
staging="$parent/.$LIBKRUN_VERSION.staging.$$"
cleanup() {
  rm -rf "$staging"
  if [ "$keep" = 0 ]; then rm -rf "$workdir"; else say "kept workdir $workdir"; fi
}
trap cleanup EXIT
run mkdir -p "$parent"
rm -rf "$staging"
run mkdir "$staging"

# manifest fields per lib: origin origin_sha256 git_commit fixups(json array)
krun_origin="" krun_osha="null" krun_commit="null" krun_fix="[]"
fw_origin="" fw_osha="null" fw_commit="null" fw_fix="[]"

stage() { # <src> <dest-name>
  run cp "$1" "$staging/$2"
  run chmod 0644 "$staging/$2"
}

macho_leaf_id() { # <file in staging>: libkrun dlopens libkrunfw by leaf name (see artifacts.rs)
  run /usr/bin/install_name_tool -id "$(basename "$1")" "$1"
  run /usr/bin/codesign --force --sign - "$1"
}

# ---- acquire
if [ "$mode" = homebrew ]; then
  for f in libkrun libkrunfw; do
    want="$LIBKRUN_VERSION"; [ "$f" = libkrunfw ] && want="$LIBKRUNFW_VERSION"
    have="$(brew list --versions "$f" 2>/dev/null | awk '{print $2}')" || true
    [ -n "$have" ] || die "Homebrew $f is not installed: run \`brew install libkrun libkrunfw\` (or use --from-source)"
    [ "$have" = "$want" ] || die "Homebrew $f is $have, pinned $want (brew upgrade/downgrade changed it); use --from-source or update the pins"
  done
  krun_src="$(brew --prefix libkrun)/$MAC_LIBKRUN_BOTTLE_FILE"
  fw_src="$(brew --prefix libkrunfw)/$MAC_LIBKRUNFW_BOTTLE_FILE"
  verify "$krun_src" "$MAC_LIBKRUN_BOTTLE_SHA256"
  verify "$fw_src" "$MAC_LIBKRUNFW_BOTTLE_SHA256"
  stage "$krun_src" "$krun_file"
  stage "$fw_src" "$fw_file"
  krun_origin=homebrew krun_osha="\"$MAC_LIBKRUN_BOTTLE_SHA256\""
  fw_origin=homebrew fw_osha="\"$MAC_LIBKRUNFW_BOTTLE_SHA256\""
else
  need git curl make
  [ -x "${CARGO_HOME:-$HOME/.cargo}/bin/cargo" ] && PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
  need cargo
  jobs="$( (nproc || sysctl -n hw.ncpu) 2>/dev/null || echo 4)"

  # libkrunfw
  if [ "$plat" = linux ] && [ "$fw_from_source" = 0 ]; then
    download "$LINUX_FW_URL" "$workdir/libkrunfw.tgz" "$LINUX_FW_SHA256"
    run mkdir -p "$workdir/fw"
    run tar -xzf "$workdir/libkrunfw.tgz" -C "$workdir/fw" "$LINUX_FW_MEMBER"
    verify "$workdir/fw/$LINUX_FW_MEMBER" "$LINUX_FW_MEMBER_SHA256"
    stage "$workdir/fw/$LINUX_FW_MEMBER" "$fw_file"
    fw_origin=release fw_osha="\"$LINUX_FW_MEMBER_SHA256\""
  else
    clone_pinned "$LIBKRUNFW_REPO" "$LIBKRUNFW_TAG" "$LIBKRUNFW_COMMIT" "$workdir/libkrunfw"
    if [ "$plat" = macos ]; then
      need clang
      download "$MAC_FW_KERNEL_URL" "$workdir/fw-kernel.tgz" "$MAC_FW_KERNEL_SHA256"
      # The bundle carries libkrunfw/kernel.c; with it present `make` only compiles the dylib.
      run tar -xzf "$workdir/fw-kernel.tgz" -C "$workdir/libkrunfw" --strip-components=1 libkrunfw/kernel.c
      run make -C "$workdir/libkrunfw" -j"$jobs"
      fw_built="$workdir/libkrunfw/libkrunfw.5.dylib"
    else
      need gcc bc bison flex cpio rsync python3
      python3 -c 'import elftools' 2>/dev/null || [ "$dry" = 1 ] || die "missing prerequisite: python3-pyelftools"
      run mkdir -p "$workdir/libkrunfw/tarballs"
      download "$FW_KERNEL_SRC_URL" "$workdir/libkrunfw/tarballs/$(basename "$FW_KERNEL_SRC_URL")" "$FW_KERNEL_SRC_SHA256"
      run make -C "$workdir/libkrunfw" -j"$jobs"
      fw_built="$workdir/libkrunfw/libkrunfw.so.$LIBKRUNFW_VERSION"
    fi
    stage "$fw_built" "$fw_file"
    fw_origin=source fw_commit="\"$LIBKRUNFW_COMMIT\""
  fi

  # libkrun
  clone_pinned "$LIBKRUN_REPO" "$LIBKRUN_TAG" "$LIBKRUN_COMMIT" "$workdir/libkrun"
  # Keep cargo's output inside the throwaway workdir (never a shared CARGO_TARGET_DIR).
  # BLK=1 / NET=1: virtio-blk (`krun_add_disk`, `krun_set_root_disk_remount`) and virtio-net
  # (`krun_add_net_unixstream`) are cargo features that upstream's Makefile leaves off by
  # default; the Homebrew bottle has both.
  run env -u CARGO_TARGET_DIR make -C "$workdir/libkrun" -j"$jobs" BLK=1 NET=1
  if [ "$plat" = macos ]; then
    stage "$workdir/libkrun/target/release/libkrun.$LIBKRUN_VERSION.dylib" "$krun_file"
  else
    need patchelf
    stage "$workdir/libkrun/target/release/libkrun.so.$LIBKRUN_VERSION" "$krun_file"
    run patchelf --set-rpath '$ORIGIN' "$staging/$krun_file"
    krun_fix='["patchelf --set-rpath $ORIGIN"]'
  fi
  check_symbols "$staging/$krun_file"
  krun_origin=source krun_commit="\"$LIBKRUN_COMMIT\""
fi

# ---- fix-ups (mirror artifacts.rs `fixups_for`)
if [ "$plat" = macos ]; then
  macho_leaf_id "$staging/$fw_file"
  fw_fix="[\"install_name_tool -id $fw_file\", \"codesign -f -s -\"]"
fi
run chmod 0444 "$staging/$krun_file" "$staging/$fw_file"

# ---- manifest + swap into place
if [ "$dry" = 1 ]; then
  say "(dry-run) would write $dest/manifest.json and swap $staging → $dest"
  [ "$netstack" = 0 ] || install_netstack
  exit 0
fi
lib_json() { # <version> <file> <origin> <origin_sha|null> <commit|null> <fixups> <path>
  printf '{\n    "version": "%s",\n    "file": "%s",\n    "origin": "%s",\n    "origin_sha256": %s,\n    "git_commit": %s,\n    "sha256": "%s",\n    "fixups": %s\n  }' \
    "$1" "$2" "$3" "$4" "$5" "$(sha256 "$7")" "$6"
}
{
  printf '{\n  "schema": %s,\n  "os": "%s",\n  "arch": "%s",\n' "$MANIFEST_SCHEMA" "$plat" "$rarch"
  printf '  "libkrun": %s,\n' "$(lib_json "$LIBKRUN_VERSION" "$krun_file" "$krun_origin" "$krun_osha" "$krun_commit" "$krun_fix" "$staging/$krun_file")"
  printf '  "libkrunfw": %s\n}\n' "$(lib_json "$LIBKRUNFW_VERSION" "$fw_file" "$fw_origin" "$fw_osha" "$fw_commit" "$fw_fix" "$staging/$fw_file")"
} > "$staging/manifest.json"
# Unmodified inputs are installed byte for byte.
[ "$krun_fix" != "[]" ] || [ "$krun_osha" = null ] || [ "\"$(sha256 "$staging/$krun_file")\"" = "$krun_osha" ] || die "libkrun copy differs from its verified input"
[ "$fw_fix" != "[]" ] || [ "$fw_osha" = null ] || [ "\"$(sha256 "$staging/$fw_file")\"" = "$fw_osha" ] || die "libkrunfw copy differs from its verified input"

old="$parent/.$LIBKRUN_VERSION.old.$$"
[ -e "$dest" ] && mv "$dest" "$old"
mv "$staging" "$dest"
rm -rf "$old"
say "installed:"
cat "$dest/manifest.json"
[ "$netstack" = 0 ] || install_netstack
