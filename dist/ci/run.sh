#!/usr/bin/env bash
# Builds and tests the release assets with Docker, the same way on a laptop and in CI:
#   run.sh [build|test|all]    assets land in $OUT_DIR (default target/release-assets/<version>)
# Containers are named sysman-rel-*; cargo's home and target live in the named volumes sysman-rel-cargo and sysman-rel-target.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
ver=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
out=${OUT_DIR:-$root/target/release-assets/$ver}

build() {
  git -C "$root" diff --quiet HEAD || echo "warning: uncommitted changes are not in the source tarball" >&2
  rm -rf "$out"
  mkdir -p "$out"
  git -C "$root" archive --prefix="sysman-$ver/" -o "$out/sysman-$ver.tar.gz" HEAD
  docker run --rm --name "sysman-rel-build-$$" \
    -v "$root:/src:ro" -v "$out:/out" \
    -v sysman-rel-cargo:/cache/cargo -v sysman-rel-target:/cache/target \
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
