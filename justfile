set shell := ["bash", "-euo", "pipefail", "-c"]

version := `sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1`
arch_out := justfile_directory() / "target/arch"
assets := justfile_directory() / "target/release-assets"

# List recipes
default:
    @just --list

# Build the binaries (release)
build:
    cargo build --release -p sysmand -p sysman -p sysman-helper

# Run the app from this checkout (release)
run: build
    ./target/release/sysman

# Run the app (debug build)
dev:
    cargo run -p sysman

# Print one meta and one sample record from the collector; writes nothing
once:
    cargo run --release -p sysmand -- --once

# Run every test in the workspace
test:
    cargo test --workspace

# Format the code
fmt:
    cargo fmt

# Check formatting and run clippy with warnings as errors
lint:
    cargo fmt --check
    cargo clippy --workspace --all-targets -- -D warnings

# Fail if two copies of gpui end up in the app's dependency graph
gpui-guard:
    ! cargo tree -d --depth 0 -p sysman | grep -q gpui

# Everything that must pass before a commit
check: lint test gpui-guard

# Build the sysman Arch package from this checkout's committed state into target/arch/pkg
package:
    #!/usr/bin/env bash
    set -euo pipefail
    git diff --quiet HEAD || echo "warning: uncommitted changes are not packaged" >&2
    rm -rf "{{ arch_out }}"
    mkdir -p "{{ arch_out }}"/{recipe,build,src,pkg}
    git archive --prefix="sysman-{{ version }}/" -o "{{ arch_out }}/src/sysman-{{ version }}.tar.gz" HEAD
    cp dist/arch/sysman/{PKGBUILD,sysman.install} "{{ arch_out }}/recipe/"
    sed -i 's/^pkgver=.*/pkgver={{ version }}/' "{{ arch_out }}/recipe/PKGBUILD"
    cd "{{ arch_out }}/recipe"
    # the local tarball stands in for the release download, so its checksum is not the published one
    BUILDDIR=../build SRCDEST=../src PKGDEST=../pkg makepkg --force --cleanbuild --skipchecksums
    ls -1 ../pkg

# Build the package, install it on this machine and (re)start the collector
install: package
    #!/usr/bin/env bash
    set -euo pipefail
    sudo pacman -U "{{ arch_out }}"/pkg/sysman-{{ version }}-*-x86_64.pkg.tar.zst
    systemctl --user daemon-reload
    systemctl --user enable sysmand
    systemctl --user restart sysmand
    # only restarts the helper if it is already running; a disabled helper stays off
    sudo systemctl try-restart sysman-helper.service
    systemctl --user --no-pager status sysmand | head -3
    echo "sysman-helper: $(systemctl is-active sysman-helper.service || true)"

# Stop the collector and remove the package
uninstall:
    systemctl --user disable --now sysmand || true
    sudo pacman -Rns sysman

# Tag v<version>, build the release assets and update both PKGBUILDs; pushes and uploads nothing
release:
    #!/usr/bin/env bash
    set -euo pipefail
    ver={{ version }} tag=v{{ version }} out="{{ assets }}/{{ version }}"
    git diff --quiet HEAD || { echo "error: commit first; a release is built from a tag" >&2; exit 1; }
    if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
        [ "$(git rev-parse "$tag^{commit}")" = "$(git rev-parse HEAD)" ] || { echo "error: $tag exists and is not HEAD" >&2; exit 1; }
    else
        git tag -a "$tag" -m "sysman $ver"
    fi
    rm -rf "$out"
    mkdir -p "$out"
    git archive --prefix="sysman-$ver/" -o "$out/sysman-$ver.tar.gz" "$tag"
    cargo build --release --locked -p sysmand -p sysman -p sysman-helper
    dist/stage.sh target/release "$out/sysman-$ver-x86_64" sysman
    tar -C "$out" -czf "$out/sysman-$ver-x86_64.tar.gz" "sysman-$ver-x86_64"
    rm -rf "$out/sysman-$ver-x86_64"
    (cd "$out" && sha256sum ./*.tar.gz > SHA256SUMS)
    src_sum=$(sha256sum "$out/sysman-$ver.tar.gz" | cut -d' ' -f1)
    bin_sum=$(sha256sum "$out/sysman-$ver-x86_64.tar.gz" | cut -d' ' -f1)
    sed -i -e "s/^pkgver=.*/pkgver=$ver/" -e "s/^pkgrel=.*/pkgrel=1/" \
        -e "s/^sha256sums=.*/sha256sums=('$src_sum')/" dist/arch/sysman/PKGBUILD
    sed -i -e "s/^pkgver=.*/pkgver=$ver/" -e "s/^pkgrel=.*/pkgrel=1/" \
        -e "s/^sha256sums_x86_64=.*/sha256sums_x86_64=('$bin_sum')/" dist/arch/sysman-bin/PKGBUILD
    for p in sysman sysman-bin; do (cd "dist/arch/$p" && makepkg --printsrcinfo > .SRCINFO); done
    echo
    echo "Release assets in $out:"
    ls -1 "$out"
    echo
    echo "Next:"
    echo "  1. git push origin main $tag"
    echo "  2. Create the GitHub release $tag and upload both tarballs and SHA256SUMS"
    echo "  3. Commit dist/arch/ (new pkgver, checksums, .SRCINFO)"
    echo "  4. Copy PKGBUILD, sysman.install and .SRCINFO from dist/arch/sysman and dist/arch/sysman-bin to their AUR repos and push"

# Collector service status
status:
    systemctl --user --no-pager status sysmand

# Follow the collector's log
logs:
    journalctl --user -u sysmand -f

# Remove build output, including packages
clean:
    cargo clean
