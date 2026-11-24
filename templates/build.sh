#!/usr/bin/env bash
# Build the curated Sandcrab templates in dependency order (base first) for one
# platform and print each resulting local image ID.
#
#   templates/build.sh                          # all four, for the Docker daemon's platform
#   templates/build.sh --platform linux/arm64   # all four, for linux/arm64 (linux/amd64 also valid)
#   templates/build.sh --pin                    # build, then write each image ID into the
#                                               # template's sandcrab.toml pins (see below)
#   templates/build.sh base ts-dev              # build a subset (base is still built first if listed)
#
# Tags: every build is tagged `sandcrab/<name>:<version>-<arch>` (amd64|arm64).
# The plain `sandcrab/<name>:<version>` tag, which manifests and backends use, is
# applied only when the target platform is the daemon's own, so a cross-arch
# build never clobbers the native image. Child templates are built FROM the
# arch-suffixed base tag (build arg BASE_IMAGE).
#
# Pins (--pin): the image ID is written to `sha256_by_arch.<arch>` in the
# `[template]` table. For amd64 it is also written to the legacy `sha256` field,
# which stays the fallback for hosts with no per-arch entry. A fully cached
# rebuild keeps the image ID (no provenance attestation), but IDs are not
# byte-reproducible from scratch yet (apt packages are unpinned), so re-run with
# --pin after any uncached rebuild.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
version="0.1.0"
order=(base python-dev ts-dev render)

daemon_arch="$(docker version --format '{{.Server.Arch}}')"
pin=0
platform=""
selected=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --pin) pin=1 ;;
    --platform) [[ $# -ge 2 ]] || { echo "--platform needs a value" >&2; exit 2; }; platform="$2"; shift ;;
    --platform=*) platform="${1#--platform=}" ;;
    -h|--help) sed -n '2,22p' "$0"; exit 0 ;;
    -*) echo "unknown option: $1" >&2; exit 2 ;;
    *) selected+=("$1") ;;
  esac
  shift
done
[[ -n "$platform" ]] || platform="linux/${daemon_arch}"
case "$platform" in
  linux/amd64) arch=amd64 ;;
  linux/arm64|linux/arm64/v8) arch=arm64; platform=linux/arm64 ;;
  *) echo "unsupported platform: $platform (expected linux/amd64 or linux/arm64)" >&2; exit 2 ;;
esac
[[ ${#selected[@]} -eq 0 ]] && selected=("${order[@]}")

for s in "${selected[@]}"; do
  [[ -d "$here/$s" ]] || { echo "unknown template: $s" >&2; exit 1; }
done
targets=()
for name in "${order[@]}"; do
  for s in "${selected[@]}"; do [[ "$s" == "$name" ]] && targets+=("$name"); done
done

# Write `id` as the `arch` pin of a sandcrab.toml: inline `sha256_by_arch` table
# in [template] (inserted after the `sha256` line, or the `image` line, if
# missing), plus the legacy `sha256` line when arch is amd64.
write_pin() {
  local manifest="$1" arch="$2" id="$3" tmp has_by_arch=0 has_sha=0
  grep -q '^sha256_by_arch = ' "$manifest" && has_by_arch=1
  grep -q '^sha256 = ' "$manifest" && has_sha=1
  tmp="$(mktemp "${manifest}.XXXXXX")"
  awk -v arch="$arch" -v id="$id" -v has_by_arch="$has_by_arch" -v has_sha="$has_sha" '
    function by_arch(line,   amd, arm, out) {
      amd = ""; arm = ""
      if (match(line, /amd64 = "[^"]*"/)) amd = substr(line, RSTART + 9, RLENGTH - 10)
      if (match(line, /arm64 = "[^"]*"/)) arm = substr(line, RSTART + 9, RLENGTH - 10)
      if (arch == "amd64") amd = id; else arm = id
      out = ""
      if (amd != "") out = "amd64 = \"" amd "\""
      if (arm != "") out = out (out == "" ? "" : ", ") "arm64 = \"" arm "\""
      done = 1
      return "sha256_by_arch = { " out " }"
    }
    /^\[/ { in_tpl = ($0 == "[template]") }
    in_tpl && /^sha256_by_arch = / { print by_arch($0); next }
    in_tpl && /^sha256 = / {
      print (arch == "amd64" ? "sha256 = \"" id "\"" : $0)
      if (!has_by_arch) print by_arch("")
      next
    }
    { print }
    in_tpl && /^image = / && !has_by_arch && !has_sha { print by_arch("") }
    END { if (!done) { print "error: no [template] image/sha256 line to pin after" > "/dev/stderr"; exit 1 } }
  ' "$manifest" > "$tmp" || { rm -f "$tmp"; return 1; }
  mv "$tmp" "$manifest"
}

for name in "${targets[@]}"; do
  arch_tag="sandcrab/${name}:${version}-${arch}"
  tags=(-t "$arch_tag")
  [[ "$arch" == "$daemon_arch" ]] && tags+=(-t "sandcrab/${name}:${version}")
  echo ">>> building ${arch_tag} (${platform})"
  # --provenance=false: the default provenance attestation embeds build
  # timestamps, so with the containerd image store every rebuild, even a fully
  # cached one, would get a new image ID and invalidate the pins.
  docker build --pull=false --provenance=false --platform "$platform" \
    --build-arg "BASE_IMAGE=sandcrab/base:${version}-${arch}" \
    "${tags[@]}" "$here/$name"
done

echo
printf '%-32s %s\n' IMAGE ID
for name in "${targets[@]}"; do
  arch_tag="sandcrab/${name}:${version}-${arch}"
  id="$(docker image inspect --format '{{.Id}}' "$arch_tag")"
  printf '%-32s %s\n' "$arch_tag" "$id"
  if [[ $pin -eq 1 ]]; then
    write_pin "$here/$name/sandcrab.toml" "$arch" "$id"
  fi
done
