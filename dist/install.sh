#!/bin/sh
# Installs or removes sysman under /usr/local on distributions without a sysman package.
#   curl -fsSL https://github.com/mah3uz/sysman/releases/latest/download/install.sh | sh
#   ./install.sh [--version X.Y.Z]        from an extracted release tarball, or downloading it
#   ./install.sh --uninstall [--purge]
set -eu

repo=mah3uz/sysman
prefix=/usr/local
manifest=$prefix/share/sysman/install-manifest
bins='sysmand sysman sysman-helper'

usage() {
    echo 'usage: install.sh [--version X.Y.Z]'
    echo '       install.sh --uninstall [--purge]'
    echo
    echo "Installs under $prefix with sudo. --purge also removes ~/.local/share/sysman."
}

die() {
    echo "error: $*" >&2
    exit 1
}

version='' uninstall=0 purge=0
while [ $# -gt 0 ]; do
    case $1 in
    --version)
        [ $# -ge 2 ] || die '--version needs a value'
        version=${2#v}
        shift
        ;;
    --uninstall) uninstall=1 ;;
    --purge) purge=1 ;;
    -h | --help)
        usage
        exit 0
        ;;
    *)
        usage >&2
        exit 2
        ;;
    esac
    shift
done
[ "$purge" = 0 ] || [ "$uninstall" = 1 ] || die '--purge only applies to --uninstall'

if [ "$(id -u)" = 0 ]; then
    sudo=
else
    command -v sudo >/dev/null 2>&1 || die 'sudo is needed to write under /usr/local'
    sudo=sudo
fi

# the account whose collector is enabled; never root
invoker=${SUDO_USER:-$(id -un)}
[ "$invoker" != root ] || invoker=
if [ -n "$invoker" ]; then
    invoker_home=$(getent passwd "$invoker" | cut -d: -f6)
fi

have_systemd() {
    [ -d /run/systemd/system ]
}

user_systemctl() {
    if [ -z "$invoker" ]; then
        return 1
    elif [ "$(id -u)" = 0 ]; then
        systemctl --user -M "$invoker@" "$@"
    else
        systemctl --user "$@"
    fi
}

if [ "$uninstall" = 1 ]; then
    [ -f "$manifest" ] || die "$manifest not found; nothing installed by install.sh to remove"
    if have_systemd; then
        user_systemctl disable --now sysmand.service 2>/dev/null || echo "note: could not stop the collector for ${invoker:-any user}; run: systemctl --user disable --now sysmand"
        $sudo systemctl disable --now sysman-helper.service 2>/dev/null || true
    else
        echo 'note: systemd is not running; no services were stopped or disabled'
    fi
    while IFS= read -r f; do
        $sudo rm -f "$prefix/$f"
    done <"$manifest"
    $sudo rm -f "$manifest"
    for d in share/doc/sysman share/licenses/sysman share/sysman; do
        $sudo rmdir "$prefix/$d" 2>/dev/null || true
    done
    if have_systemd; then
        $sudo systemctl daemon-reload || true
    fi
    if [ "$purge" = 1 ]; then
        if [ -n "$invoker" ]; then
            rm -rf "$invoker_home/.local/share/sysman"
        else
            echo 'note: no non-root invoking user; ~/.local/share/sysman was left alone'
        fi
    else
        echo 'Kept ~/.local/share/sysman (history and crash log); remove it with --purge.'
    fi
    echo 'sysman removed.'
    exit 0
fi

if command -v pacman >/dev/null 2>&1 && pacman -Qq sysman >/dev/null 2>&1; then
    die 'sysman is installed by pacman; remove it first or keep using it'
fi
if command -v pacman >/dev/null 2>&1 && pacman -Qq sysman-bin >/dev/null 2>&1; then
    die 'sysman-bin is installed by pacman; remove it first or keep using it'
fi
if command -v dpkg-query >/dev/null 2>&1 &&
    dpkg-query -W -f='${Status}' sysman 2>/dev/null | grep -q 'install ok installed'; then
    die 'sysman is installed by dpkg; remove it first or keep using it'
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

here=$(dirname "$0")
if [ -z "$version" ] && [ -f "$0" ] && [ -x "$here/usr/bin/sysmand" ]; then
    src=$here/usr
else
    command -v curl >/dev/null 2>&1 || die 'curl is needed to download the release'
    if [ -n "$version" ]; then
        base=https://github.com/$repo/releases/download/v$version
    else
        base=https://github.com/$repo/releases/latest/download
    fi
    echo "Downloading from $base"
    curl -fsSL -o "$work/SHA256SUMS" "$base/SHA256SUMS" || die 'could not download SHA256SUMS'
    tarball=$(sed -n 's/^[0-9a-f]*  \(sysman-[^ ]*-x86_64\.tar\.gz\)$/\1/p' "$work/SHA256SUMS")
    [ -n "$tarball" ] || die 'SHA256SUMS lists no x86_64 tarball'
    curl -fsSL -o "$work/$tarball" "$base/$tarball" || die "could not download $tarball"
    (cd "$work" && grep "  $tarball\$" SHA256SUMS | sha256sum -c -) || die 'checksum mismatch'
    tar -C "$work" -xzf "$work/$tarball"
    src=$work/${tarball%.tar.gz}/usr
fi
[ -x "$src/bin/sysmand" ] || die "no sysmand under $src; is this a release tarball?"

if getent group wheel >/dev/null 2>&1; then
    group=wheel
elif getent group sudo >/dev/null 2>&1; then
    group=sudo
else
    group=root
    echo 'warning: neither a wheel nor a sudo group exists; only root can use the helper until you create one'
    echo "         and set Group= and --group in $prefix/lib/systemd/system/sysman-helper.service"
fi

stage=$work/stage
mkdir "$stage"
cp -R "$src/." "$stage/"
for unit in "$stage/lib/systemd/user/sysmand.service" "$stage/lib/systemd/system/sysman-helper.service"; do
    sed -e "s|^ExecStart=/usr/bin/|ExecStart=$prefix/bin/|" \
        -e "s|^Group=.*|Group=$group|" \
        -e "s|--group [^ ]*|--group $group|" "$unit" >"$unit.new"
    mv "$unit.new" "$unit"
    chmod 644 "$unit"
done
mkdir -p "$stage/share/sysman"
(cd "$stage" && find . -type f | sed 's|^\./||' | sort) >"$work/manifest"
echo share/sysman/install-manifest >>"$work/manifest"
cp "$work/manifest" "$stage/share/sysman/install-manifest"

$sudo mkdir -p "$prefix"
$sudo cp -R "$stage/." "$prefix/"
echo "Installed under $prefix (helper group: $group)"

if command -v ldd >/dev/null 2>&1; then
    for b in $bins; do
        missing=$(ldd "$prefix/bin/$b" 2>&1 | sed -n 's/^[[:space:]]*\(.*\) => not found.*/\1/p')
        if [ -n "$missing" ]; then
            echo "warning: $b needs shared libraries that are not installed:"
            echo "$missing" | sed 's/^/    /'
        fi
    done
fi

if have_systemd; then
    $sudo systemctl daemon-reload || true
    if [ -z "$invoker" ]; then
        echo 'note: running as root with no invoking user; enable the collector yourself with: systemctl --user enable --now sysmand'
    else
        user_systemctl daemon-reload || true
        user_systemctl enable --now sysmand.service ||
            echo 'warning: could not enable the collector; run: systemctl --user enable --now sysmand'
    fi
else
    echo 'note: systemd is not running; units were installed but nothing was enabled or started'
fi

echo
echo 'Optional privileged helper (SMART health, CPU power, per-process I/O and network):'
echo '    sudo systemctl enable --now sysman-helper'
