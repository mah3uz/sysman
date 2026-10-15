#!/usr/bin/env bash
# Lays out the installed file tree:
#   stage.sh <dir holding sysman, sysmand and sysman-helper> <destdir> <package name> [helper group, default wheel] [prefix, default /usr]
set -euo pipefail
bin=$1 dest=$2 name=$3 group=${4:-wheel} prefix=${5:-/usr}
root=$(cd "$(dirname "$0")/.." && pwd)

d=$dest$prefix

install -Dm755 "$bin/sysmand" "$bin/sysman" "$bin/sysman-helper" -t "$d/bin/"
sed "s|^ExecStart=.*|ExecStart=$prefix/bin/sysmand|" "$root/dist/sysmand.service" \
  | install -Dm644 /dev/stdin "$d/lib/systemd/user/sysmand.service"
sed -e "s|^ExecStart=.*|ExecStart=$prefix/bin/sysman-helper --group $group|" -e "s|^Group=.*|Group=$group|" \
  "$root/dist/sysman-helper.service" | install -Dm644 /dev/stdin "$d/lib/systemd/system/sysman-helper.service"
install -Dm644 "$root/dist/sysman.desktop" -t "$d/share/applications/"
install -Dm644 "$root/assets/logo.svg" "$d/share/icons/hicolor/scalable/apps/sysman.svg"
for size in 16 22 24 32 48 64 128 256 512; do
  install -dm755 "$d/share/icons/hicolor/${size}x${size}/apps"
  rsvg-convert -w "$size" -h "$size" "$root/assets/logo.svg" \
    -o "$d/share/icons/hicolor/${size}x${size}/apps/sysman.png"
done
install -Dm644 "$root/LICENSE" -t "$d/share/licenses/$name/"
install -Dm644 "$root/README.md" -t "$d/share/doc/$name/"
