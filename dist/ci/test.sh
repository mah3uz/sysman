#!/usr/bin/env bash
# Installs the built assets in fresh containers: test.sh <assets dir>
set -euo pipefail

assets=$(cd "$1" && pwd)
deb=$(basename "$assets"/sysman_*_amd64.deb)

for image in ubuntu:22.04 debian:12 ubuntu:24.04; do
  echo "== deb on $image"
  docker run --rm --name "sysman-rel-deb-${image//[:.]/-}-$$" -v "$assets:/assets:ro" -e DEB="$deb" "$image" sh -ec '
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq "/assets/$DEB" >/dev/null
    for b in sysman sysmand sysman-helper; do
      if ldd "/usr/bin/$b" | grep "not found"; then echo "error: $b has missing libraries" >&2; exit 1; fi
    done
    grep -q "^Group=sudo$" /usr/lib/systemd/system/sysman-helper.service
    sysmand --help | head -3
    apt-get remove -y -qq sysman >/dev/null
    test ! -e /usr/bin/sysmand
  '
done

echo "== install.sh from the tarball, then --uninstall"
docker run --rm --name "sysman-rel-installsh-$$" -v "$assets:/assets:ro" ubuntu:22.04 sh -ec '
  mkdir /tmp/x
  tar -C /tmp/x -xzf /assets/sysman-*-x86_64.tar.gz
  /tmp/x/sysman-*-x86_64/install.sh | tee /tmp/install.log
  grep -q "systemd is not running" /tmp/install.log
  grep -q "^Group=sudo$" /usr/local/lib/systemd/system/sysman-helper.service
  grep -q "^ExecStart=/usr/local/bin/sysman-helper --group sudo$" /usr/local/lib/systemd/system/sysman-helper.service
  grep -q "^ExecStart=/usr/local/bin/sysmand$" /usr/local/lib/systemd/user/sysmand.service
  test -x /usr/local/bin/sysman
  /usr/local/bin/sysmand --help | head -3
  mkdir -p /root/.local/share/sysman
  /tmp/x/sysman-*-x86_64/install.sh --uninstall | tee /tmp/uninstall.log
  grep -q "systemd is not running" /tmp/uninstall.log
  test ! -e /usr/local/bin/sysmand
  test ! -e /usr/local/lib/systemd/system/sysman-helper.service
  test ! -e /usr/local/share/sysman
  test -d /root/.local/share/sysman
'
echo "all container tests passed"
