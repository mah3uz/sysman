#!/usr/bin/env bash
# Lays out the installed file tree: stage.sh <dir holding sysman and sysmand> <destdir> <package name>
set -euo pipefail
bin=$1 dest=$2 name=$3
root=$(cd "$(dirname "$0")/.." && pwd)

install -Dm755 "$bin/sysmand" "$bin/sysman" -t "$dest/usr/bin/"
sed 's|^ExecStart=.*|ExecStart=/usr/bin/sysmand|' "$root/dist/sysmand.service" \
  | install -Dm644 /dev/stdin "$dest/usr/lib/systemd/user/sysmand.service"
install -Dm644 "$root/dist/sysman.desktop" -t "$dest/usr/share/applications/"
install -Dm644 "$root/assets/logo.svg" "$dest/usr/share/icons/hicolor/scalable/apps/sysman.svg"
for size in 16 22 24 32 48 64 128 256 512; do
  install -dm755 "$dest/usr/share/icons/hicolor/${size}x${size}/apps"
  rsvg-convert -w "$size" -h "$size" "$root/assets/logo.svg" \
    -o "$dest/usr/share/icons/hicolor/${size}x${size}/apps/sysman.png"
done
install -Dm644 "$root/LICENSE" -t "$dest/usr/share/licenses/$name/"
install -Dm644 "$root/README.md" -t "$dest/usr/share/doc/$name/"
