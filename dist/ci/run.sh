#!/usr/bin/env bash
# Builds and tests the release assets with Docker, the same way on a laptop and in CI:
#   run.sh [build|test|all]    assets land in $OUT_DIR (default target/release-assets/<version>)
# Containers are named sysman-rel-*; cargo's home and target live in the named volumes sysman-rel-cargo and sysman-rel-target,
# or under $CACHE_DIR when it is set, so CI can save them between runs.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
ver=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
out=${OUT_DIR:-$root/target/release-assets/$ver}

build() {
  rm -rf "$out"
  mkdir -p "$out"
  local cargo=sysman-rel-cargo target=sysman-rel-target
  if [ -n "${CACHE_DIR:-}" ]; then
    mkdir -p "$CACHE_DIR/cargo" "$CACHE_DIR/target"
    cargo=$CACHE_DIR/cargo target=$CACHE_DIR/target
  fi
  docker run --rm --name "sysman-rel-build-$$" \
    -v "$root:/src:ro" -v "$out:/out" \
    -v "$cargo:/cache/cargo" -v "$target:/cache/target" \
    -e CARGO_HOME=/cache/cargo -e RUSTUP_HOME=/cache/cargo/rustup -e CARGO_TARGET_DIR=/cache/target \
    -e CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-3}" -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
    ubuntu:22.04 /src/dist/ci/build.sh
}

case ${1:-all} in
build) build ;;
test) "$root/dist/ci/test.sh" "$out" ;;
all)
  build
  "$root/dist/ci/test.sh" "$out"
  ;;
*)
  echo "usage: run.sh [build|test|all]" >&2
  exit 2
  ;;
esac
