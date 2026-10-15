set shell := ["bash", "-euo", "pipefail", "-c"]

arch_out := justfile_directory() / "target/arch"

# List recipes
default:
    @just --list

# Build both binaries (release)
build:
    cargo build --release -p sysmand -p sysman

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

# Build the Arch package from the committed state into target/arch/pkg
package:
    #!/usr/bin/env bash
    set -euo pipefail
    git diff --quiet HEAD || echo "warning: uncommitted changes are not packaged" >&2
    mkdir -p "{{ arch_out }}"/{build,src,pkg}
    cd dist/arch
    BUILDDIR="{{ arch_out }}/build" SRCDEST="{{ arch_out }}/src" PKGDEST="{{ arch_out }}/pkg" makepkg --force --cleanbuild
    ls -1 "{{ arch_out }}/pkg"

# Build the package, install it on this machine and (re)start the collector
install: package
    #!/usr/bin/env bash
    set -euo pipefail
    pkg=$(ls -t "{{ arch_out }}"/pkg/sysman-git-[0-9]*.pkg.tar.zst | head -1)
    sudo pacman -U "$pkg"
    systemctl --user daemon-reload
    systemctl --user enable sysmand
    systemctl --user restart sysmand
    systemctl --user --no-pager status sysmand | head -3

# Stop the collector and remove the package
uninstall:
    systemctl --user disable --now sysmand || true
    sudo pacman -Rns sysman-git

# Collector service status
status:
    systemctl --user --no-pager status sysmand

# Follow the collector's log
logs:
    journalctl --user -u sysmand -f

# Remove build output, including packages
clean:
    cargo clean
