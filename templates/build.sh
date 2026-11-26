#!/usr/bin/env bash
# Build the curated Sandcrab templates in dependency order (base first) for one
# platform, print each resulting local image ID and pin it on this host.
#
#   templates/build.sh                          # all four, for the Docker daemon's platform
#   templates/build.sh --platform linux/arm64   # all four, for linux/arm64 (linux/amd64 also valid)
#   templates/build.sh base ts-dev              # build a subset (base is still built first if listed)
#   templates/build.sh --no-pin base            # build without recording host-local pins
#
# Templates with `external = true` (an image built elsewhere, no Dockerfile) are
# skipped: pull the image and adopt it with `sandcrab template pin <name>`.
# Tags: every build is tagged `sandcrab/<name>:<version>-<arch>` (amd64|arm64).
# The plain `sandcrab/<name>:<version>` tag, which manifests and backends use, is
# applied only when the target platform is the daemon's own, so a cross-arch
# build never clobbers the native image. Child templates are built FROM the
# arch-suffixed base tag (build arg BASE_IMAGE).
#
# Pins: each built image ID is recorded HOST-LOCALLY (never in the committed
# sandcrab.toml), one JSON file per template, version and arch:
#   $SANDCRAB_TEMPLATE_PINS/<name>/<version>/<arch>.json
#   (default: ${SANDCRAB_HOME:-~/.sandcrab}/template-pins)
# Spawns verify the local image against it. This is LOCAL INTEGRITY ("unchanged
# since this host built it"), not provenance: image IDs do not reproduce across
# hosts (unpinned apt packages, layer timestamps, classic vs containerd image
# stores). The Dockerfile FROM bases are digest-pinned. `--pin` is accepted for
# compatibility and does nothing (pinning is the default).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
version="0.1.0"
order=(base python-dev ts-dev render)

daemon_arch="$(docker version --format '{{.Server.Arch}}')"
pin=1
platform=""
selected=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --pin) ;;  # deprecated: pinning is the default
    --no-pin) pin=0 ;;
    --platform) [[ $# -ge 2 ]] || { echo "--platform needs a value" >&2; exit 2; }; platform="$2"; shift ;;
    --platform=*) platform="${1#--platform=}" ;;
    -h|--help) sed -n '2,26p' "$0"; exit 0 ;;
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

# `external = true` in a manifest's [template] table: the image is built elsewhere.
is_external() {
  awk '/^\[/ { in_tpl = ($0 == "[template]") } in_tpl && /^external *= *true *(#.*)?$/ { found = 1 } END { exit !found }' "$1"
}

for s in "${selected[@]}"; do
  [[ -d "$here/$s" ]] || { echo "unknown template: $s" >&2; exit 1; }
  if [[ -f "$here/$s/sandcrab.toml" ]] && is_external "$here/$s/sandcrab.toml"; then
    echo "skipping $s: external template (pull its image, then \`sandcrab template pin $s\`)" >&2
  fi
done
targets=()
for name in "${order[@]}"; do
  is_external "$here/$name/sandcrab.toml" && continue
  for s in "${selected[@]}"; do [[ "$s" == "$name" ]] && targets+=("$name"); done
done

# Host-local pin store; same resolution as sandcrab-core `pins::default_pins_dir`.
if [[ -n "${SANDCRAB_TEMPLATE_PINS:-}" ]]; then
  pins_dir="$SANDCRAB_TEMPLATE_PINS"
else
  pins_dir="${SANDCRAB_HOME:-${HOME:?HOME or SANDCRAB_HOME must be set}/.sandcrab}/template-pins"
fi

# The `version` of a template's [template] table.
manifest_version() {
  awk '/^\[/ { in_tpl = ($0 == "[template]") } in_tpl && /^version = "/ { gsub(/^version = "|"$/, ""); print; exit }' "$1"
}

# Record `id` as the host-local pin for template `name` at `ver` / `arch`
# (atomic: temp file + rename). Values are validated first, so no JSON escaping
# is needed.
record_pin() {
  local name="$1" ver="$2" arch="$3" image="$4" id="$5" dir tmp
  [[ "$name" =~ ^[a-z0-9][a-z0-9_-]*$ ]] || { echo "bad template name: $name" >&2; return 1; }
  [[ "$ver" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.+-]+)?$ ]] || { echo "bad version for $name: $ver" >&2; return 1; }
  [[ "$id" =~ ^sha256:[0-9a-f]{64}$ ]] || { echo "unexpected image id for $image: $id" >&2; return 1; }
  [[ "$image" =~ ^[A-Za-z0-9./:_-]+$ ]] || { echo "bad image reference: $image" >&2; return 1; }
  dir="$pins_dir/$name/$ver"
  (umask 077 && mkdir -p "$dir")
  tmp="$(mktemp "$dir/.${arch}.json.XXXXXX")"
  printf '{\n  "schema": 1,\n  "template": "%s",\n  "version": "%s",\n  "arch": "%s",\n  "image": "%s",\n  "image_id": "%s",\n  "source": "build",\n  "recorded_at": "%s"\n}\n' \
    "$name" "$ver" "$arch" "$image" "$id" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$tmp"
  mv "$tmp" "$dir/$arch.json"
}

# Tags and pins use one version; refuse a manifest that disagrees.
for name in "${targets[@]}"; do
  ver="$(manifest_version "$here/$name/sandcrab.toml")"
  [[ "$ver" == "$version" ]] || { echo "$name/sandcrab.toml version '$ver' != build.sh version '$version'" >&2; exit 1; }
done

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
    record_pin "$name" "$version" "$arch" "$arch_tag" "$id"
  fi
done
if [[ $pin -eq 1 ]]; then
  echo
  echo "pinned on this host (local integrity, not provenance): $pins_dir"
fi
