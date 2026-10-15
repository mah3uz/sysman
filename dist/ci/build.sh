#!/usr/bin/env bash
# Builds the release assets inside an ubuntu:22.04 container (run by dist/ci/run.sh):
#   /src  the checkout (read only is fine)
#   /out  receives the assets and SHA256SUMS
# Optional: CARGO_HOME and CARGO_TARGET_DIR on cached volumes, CARGO_BUILD_JOBS, HOST_UID/HOST_GID to own /out.
set -euo pipefail

src=/src out=/out
glibc_max=2.35
maintainer='Mahfuz Shaikh <mah3uz@gmail.com>'

# libraries GPUI and wgpu open with dlopen at run time, which dpkg-shlibdeps cannot see
dlopen_depends=(libwayland-client0 libwayland-egl1 libvulkan1 libegl1)

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends \
  ca-certificates curl git build-essential pkg-config binutils dpkg-dev \
  clang llvm libclang-dev libbpf-dev \
  autoconf automake autopoint libtool m4 gawk flex bison gettext \
  libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev libxcb1-dev libx11-xcb-dev \
  libfontconfig-dev libfreetype-dev librsvg2-bin >/dev/null

# clang -target bpf does not search the multiarch include directory
ln -sfn x86_64-linux-gnu/asm /usr/include/asm

export CARGO_HOME=${CARGO_HOME:-/root/.cargo} RUSTUP_HOME=${RUSTUP_HOME:-/root/.rustup}
export PATH=$CARGO_HOME/bin:$PATH
if ! command -v rustc >/dev/null; then
  curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
fi
rustup component add rustfmt

cd "$src"
ver=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/target}
cargo build --release --locked -p sysmand -p sysman -p sysman-helper --features sysman-helper/static-libbpf
bins=$CARGO_TARGET_DIR/release

work=$(mktemp -d)
mkdir "$work/bin"
for b in sysmand sysman sysman-helper; do
  strip --strip-unneeded -o "$work/bin/$b" "$bins/$b"
done

echo "== glibc symbol versions (limit $glibc_max)"
for b in sysmand sysman sysman-helper; do
  max=$(objdump -T "$work/bin/$b" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -V | tail -1)
  echo "$b: GLIBC_$max"
  if [ "$(printf '%s\n%s\n' "$max" "$glibc_max" | sort -V | tail -1)" != "$glibc_max" ]; then
    echo "error: $b needs GLIBC_$max, above $glibc_max" >&2
    exit 1
  fi
done

generic=sysman-$ver-x86_64
"$src/dist/stage.sh" "$work/bin" "$work/$generic" sysman wheel /usr
install -m755 "$src/dist/install.sh" "$work/$generic/install.sh"
tar -C "$work" --owner=0 --group=0 -czf "$out/$generic.tar.gz" "$generic"
install -m755 "$src/dist/install.sh" "$out/install.sh"

deb=$work/deb
"$src/dist/stage.sh" "$work/bin" "$deb" sysman sudo /usr
install -Dm644 "$src/LICENSE" "$deb/usr/share/doc/sysman/copyright"
rm -r "$deb/usr/share/licenses"
mkdir "$deb/DEBIAN" "$work/debian"
install -m755 "$src"/dist/deb/{postinst,prerm,postrm} "$deb/DEBIAN/"
touch "$work/debian/control"
shlibs=$(cd "$work" && dpkg-shlibdeps -O -e"$work/bin/sysmand" -e"$work/bin/sysman" -e"$work/bin/sysman-helper" |
  sed 's/^shlibs:Depends=//')
depends=$shlibs
for p in "${dlopen_depends[@]}"; do depends+=", $p"; done
depends+=", iproute2, fontconfig"
size=$(du -sk "$deb/usr" | cut -f1)
cat >"$deb/DEBIAN/control" <<EOF
Package: sysman
Version: $ver-1
Architecture: amd64
Maintainer: $maintainer
Installed-Size: $size
Depends: $depends
Recommends: smartmontools
Section: utils
Priority: optional
Homepage: https://github.com/mah3uz/sysman
Description: System monitor for Linux
 A metrics collector daemon with crash log and history, and a GPUI desktop app.
 An optional privileged helper adds SMART drive health, CPU package power,
 per-process disk I/O and network traffic.
EOF
echo "== Depends: $depends"
dpkg-deb --root-owner-group -Zxz --build "$deb" "$out/sysman_$ver-1_amd64.deb"

(cd "$out" && sha256sum "$generic.tar.gz" "sysman_$ver-1_amd64.deb" install.sh >SHA256SUMS)
if [ -n "${HOST_UID:-}" ]; then
  chown -R "$HOST_UID:${HOST_GID:-$HOST_UID}" "$out"
fi
ls -l "$out"
