set shell := ["bash", "-euo", "pipefail", "-c"]

version := `sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1`
arch_out := justfile_directory() / "target/arch"

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
    cargo clippy --workspace --all-targets --release -- -D warnings

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
    # the package's upgrade hook restarts a running collector and helper; this covers a first install
    systemctl --user daemon-reload
    systemctl --user enable --now sysmand
    systemctl --user --no-pager status sysmand | head -3
    echo "sysman-helper: $(systemctl is-active sysman-helper.service || true)"

# Stop the collector and remove the package
uninstall:
    systemctl --user disable --now sysmand || true
    sudo pacman -Rns sysman

# Set the version to VER (x.y.z), check, tag and push v<VER>, then follow the CI run that drafts the GitHub release
release ver:
    #!/usr/bin/env bash
    set -euo pipefail
    ver={{ ver }} tag=v{{ ver }} current={{ version }}
    [[ $ver =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "error: give the version as x.y.z, e.g. just release 0.2.0" >&2; exit 1; }
    [ "$(git branch --show-current)" = main ] || { echo "error: release from main" >&2; exit 1; }
    [ -z "$(git status --porcelain)" ] || { echo "error: uncommitted changes; a release is cut from a clean tree" >&2; exit 1; }
    ! git rev-parse -q --verify "refs/tags/$tag" >/dev/null || { echo "error: tag $tag already exists" >&2; exit 1; }
    git fetch -q origin main
    git merge-base --is-ancestor origin/main HEAD || { echo "error: origin/main has commits this checkout lacks; pull first" >&2; exit 1; }
    if [ "$ver" != "$current" ]; then
        echo "Version $current -> $ver"
        sed -i "0,/^version = \".*\"/s//version = \"$ver\"/" Cargo.toml
        cargo update -w --offline -q
    fi
    just check
    if [ -n "$(git status --porcelain)" ]; then git commit -qam "sysman $ver"; fi
    git tag -a "$tag" -m "sysman $ver"
    git push -q origin main "$tag"
    echo "Pushed $tag; waiting for the release workflow to start..."
    run=
    for _ in $(seq 30); do
        run=$(gh run list --workflow release.yml --event push --limit 10 --json databaseId,headBranch \
            --jq ".[] | select(.headBranch == \"$tag\") | .databaseId" | head -1)
        [ -n "$run" ] && break
        sleep 5
    done
    [ -n "$run" ] || { echo "error: no release run for $tag after 150 s; see the Actions tab on GitHub" >&2; exit 1; }
    if ! gh run watch "$run" --exit-status --interval 15; then
        echo "error: the release workflow failed: $(gh run view "$run" --json url --jq .url)" >&2
        exit 1
    fi
    echo
    echo "Draft release ready: $(gh release view "$tag" --json url --jq .url)"
    echo "Review and publish it on GitHub, then run: just aur"

# After the GitHub release is published: fill both PKGBUILDs from its SHA256SUMS and commit them in clones of the AUR repos (does not push)
aur pkgrel="1":
    #!/usr/bin/env bash
    set -euo pipefail
    ver={{ version }} rel={{ pkgrel }}
    sums=$(curl -fsSL "https://github.com/mah3uz/sysman/releases/download/v$ver/SHA256SUMS")
    src_sum=$(awk -v f="sysman-$ver.tar.gz" '$2 == f {print $1}' <<<"$sums")
    bin_sum=$(awk -v f="sysman-$ver-x86_64.tar.gz" '$2 == f {print $1}' <<<"$sums")
    [ -n "$src_sum" ] && [ -n "$bin_sum" ] || { echo "error: SHA256SUMS lacks a tarball checksum" >&2; exit 1; }
    sed -i -e "s/^pkgver=.*/pkgver=$ver/" -e "s/^pkgrel=.*/pkgrel=$rel/" \
        -e "s/^sha256sums=.*/sha256sums=('$src_sum')/" dist/arch/sysman/PKGBUILD
    sed -i -e "s/^pkgver=.*/pkgver=$ver/" -e "s/^pkgrel=.*/pkgrel=$rel/" \
        -e "s/^sha256sums_x86_64=.*/sha256sums_x86_64=('$bin_sum')/" dist/arch/sysman-bin/PKGBUILD
    mkdir -p target/aur
    for p in sysman sysman-bin; do
        (cd "dist/arch/$p" && makepkg --printsrcinfo > .SRCINFO)
        dir=target/aur/$p
        if [ -d "$dir/.git" ]; then
            if [ -n "$(git -C "$dir" ls-remote --heads origin)" ]; then git -C "$dir" pull --ff-only; fi
        else
            git clone -b master "ssh://aur@aur.archlinux.org/$p.git" "$dir"
        fi
        cp "dist/arch/$p/PKGBUILD" "dist/arch/$p/.SRCINFO" "dist/arch/$p/sysman.install" "$dir/"
        git -C "$dir" add PKGBUILD .SRCINFO sysman.install
        if git -C "$dir" diff --cached --quiet; then
            echo "$p: nothing to commit"
        else
            git -C "$dir" commit -qm "Update to $ver-$rel"
            echo "$p: committed; push with: git -C $dir push"
        fi
    done
    echo "Commit the updated dist/arch/ files in this repository too."

# Collector service status
status:
    systemctl --user --no-pager status sysmand

# Follow the collector's log
logs:
    journalctl --user -u sysmand -f

# Remove build output, including packages
clean:
    cargo clean

# Run the website's dev server
site:
    cd docs/site && npm install && npm run dev

# Build the website and serve the production build locally
site-preview:
    cd docs/site && npm ci && npm run build && npm run preview

# Build and deploy the website to Cloudflare (needs `npx wrangler login` once)
site-deploy:
    cd docs/site && npm ci && npm run deploy
