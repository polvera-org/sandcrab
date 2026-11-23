#!/usr/bin/env bash
# Build all curated Sandcrab templates in dependency order (base first) and
# print each resulting local image ID.
#
#   templates/build.sh            # build all four
#   templates/build.sh --pin      # build, then write each image ID into the
#                                 # template's sandcrab.toml `sha256` field
#   templates/build.sh base ts-dev  # build a subset (base is still built first if listed)
#
# Note: the recorded `sha256` pins the *locally built* image ID. Image IDs are
# not byte-reproducible across rebuilds yet (apt packages are unpinned), so
# re-run with --pin after any rebuild.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
version="0.1.0"
order=(base python-dev ts-dev render)

pin=0
selected=()
for arg in "$@"; do
  case "$arg" in
    --pin) pin=1 ;;
    -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
    *) selected+=("$arg") ;;
  esac
done
[[ ${#selected[@]} -eq 0 ]] && selected=("${order[@]}")

targets=()
for name in "${order[@]}"; do
  for s in "${selected[@]}"; do [[ "$s" == "$name" ]] && targets+=("$name"); done
done
for s in "${selected[@]}"; do
  [[ -d "$here/$s" ]] || { echo "unknown template: $s" >&2; exit 1; }
done

for name in "${targets[@]}"; do
  tag="sandcrab/${name}:${version}"
  echo ">>> building ${tag}"
  docker build --pull=false -t "$tag" "$here/$name"
done

echo
printf '%-26s %s\n' IMAGE ID
for name in "${targets[@]}"; do
  tag="sandcrab/${name}:${version}"
  id="$(docker image inspect --format '{{.Id}}' "$tag")"
  printf '%-26s %s\n' "$tag" "$id"
  if [[ $pin -eq 1 ]]; then
    manifest="$here/$name/sandcrab.toml"
    if grep -q '^sha256 = ' "$manifest"; then
      sed -i.bak "s|^sha256 = .*|sha256 = \"${id}\"|" "$manifest" && rm -f "$manifest.bak"
    else
      echo "warning: no sha256 line in $manifest" >&2
    fi
  fi
done
