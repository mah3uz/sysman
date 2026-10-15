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

# The whole release, e.g. `just release 0.2.0`: checks, version bump and tag, then `just publish`; GitHub builds the rest
release ver:
    #!/usr/bin/env bash
    set -euo pipefail
    v={{ ver }} tag=v{{ ver }} current={{ version }}
    fail() { echo "release: $*" >&2; exit 1; }
    last=$(git describe --tags --abbrev=0 --match 'v*' --exclude 'v*-*' 2>/dev/null || true)
    last=${last#v}
    last=${last:-0.0.0}

    [[ $v =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "give the version as x.y.z, e.g. just release 0.2.0"
    [[ $v != "$last" && $(printf '%s\n%s\n' "$last" "$v" | sort -V | tail -1) == "$v" ]] || fail "$v is not newer than the last release ($last)"
    [[ -n $(dist/release-notes.sh Unreleased) ]] || fail "CHANGELOG.md has nothing under ## Unreleased"
    [[ $(git branch --show-current) == main ]] || fail "switch to main first"
    [[ -z $(git status --porcelain) ]] || fail "commit or stash your changes first"
    git fetch -q origin
    [[ $(git rev-list --count HEAD..origin/main) == 0 ]] || fail "main is behind origin/main; pull first"
    ! git rev-parse -q --verify "refs/tags/$tag" >/dev/null || fail "tag $tag already exists"
    ! git ls-remote --exit-code --tags origin "$tag" >/dev/null || fail "tag $tag already exists on origin"
    gh auth status >/dev/null 2>&1 || fail "gh is not logged in; run gh auth login"
    ssh -o BatchMode=yes aur@aur.archlinux.org help >/dev/null 2>&1 || fail "can't reach the AUR over SSH; add your key at https://aur.archlinux.org/account"
    (cd docs/site && { [ -d node_modules ] || npm ci --silent; } && npx wrangler whoami 2>&1 | grep -q "You are logged in") ||
        fail "wrangler is not logged in, and the website is deployed at the end; run: cd docs/site && npx wrangler login"

    echo "==> lint and tests"
    just check

    echo "==> version $v (last release: $last)"
    if [[ $v != "$current" ]]; then
        sed -i "0,/^version = \".*\"/s//version = \"$v\"/" Cargo.toml
        cargo update -w --offline -q
    fi
    sed -i -e "s/^pkgver=.*/pkgver=$v/" -e "s/^pkgrel=.*/pkgrel=1/" -e "s/^sha256sums=.*/sha256sums=('SKIP')/" dist/arch/sysman/PKGBUILD
    sed -i -e "s/^pkgver=.*/pkgver=$v/" -e "s/^pkgrel=.*/pkgrel=1/" -e "s/^sha256sums_x86_64=.*/sha256sums_x86_64=('SKIP')/" dist/arch/sysman-bin/PKGBUILD
    sed -i "s/^## Unreleased$/## Unreleased\n\n## $v - $(date +%F)/" CHANGELOG.md
    git commit -qam "sysman $v"

    # Everything after this is public and can't be taken back.
    read -rp "Push $tag, publish the GitHub release and AUR packages, and deploy the website? [y/N] " answer
    if [[ $answer != [yY] ]]; then
        echo "Stopped before pushing. To undo the version commit: git reset --hard HEAD~1"
        exit 1
    fi

    echo "==> tag and push"
    git tag -a "$tag" -m "sysman $v"
    git push -q origin main "$tag"
    just publish "$v"

# After v<VER> is pushed: build the Arch tarball from the tag, publish the GitHub release with its notes, push the AUR packages and deploy the website
publish ver:
    #!/usr/bin/env bash
    set -euo pipefail
    v={{ ver }} tag=v{{ ver }} root={{ justfile_directory() }}
    fail() { echo "publish: $*" >&2; exit 1; }
    trap 'echo "publish: stopped; fix the cause and run just publish $v again" >&2' ERR
    [[ $(git branch --show-current) == main ]] || fail "switch to main first"
    [[ -z $(git status --porcelain) ]] || fail "commit or stash your changes first"
    git ls-remote --exit-code --tags origin "$tag" >/dev/null || fail "push $tag first"
    grep -q "^pkgver=$v$" dist/arch/sysman/PKGBUILD || fail "dist/arch is not at $v"
    notes=$(dist/release-notes.sh "$v")

    name=sysman-$v-arch-x86_64
    out=$root/target/publish/$v
    echo "==> Arch binaries from $tag"
    rm -rf "$out"
    mkdir -p "$out/src" "$out/bin"
    git archive "$tag" | tar -x -C "$out/src"
    # a target dir of its own, kept between releases, so later builds are incremental
    (cd "$out/src" && CARGO_TARGET_DIR="$root/target/publish-build" \
        cargo build --release --locked -p sysmand -p sysman -p sysman-helper --features sysman-helper/static-libbpf)
    for b in sysmand sysman sysman-helper; do
        strip --strip-unneeded -o "$out/bin/$b" "$root/target/publish-build/release/$b"
    done
    "$out/src/dist/stage.sh" "$out/bin" "$out/$name" sysman wheel /usr
    tar -C "$out" --owner=0 --group=0 -czf "$out/$name.tar.gz" "$name"

    echo "==> GitHub release"
    if gh release view "$tag" >/dev/null 2>&1; then
        gh release edit "$tag" --notes "$notes" >/dev/null
        gh release upload "$tag" --clobber "$out/$name.tar.gz"
    else
        # GitHub marks it latest once its own assets are up, so install.sh never meets a half-filled release
        gh release create "$tag" --verify-tag --title "sysman $v" --notes "$notes" --latest=false "$out/$name.tar.gz" ||
            gh release upload "$tag" --clobber "$out/$name.tar.gz"
    fi

    echo "==> AUR checksums"
    src_sum=$(curl -fsSL --retry 6 --retry-delay 5 --retry-all-errors "https://github.com/mah3uz/sysman/archive/refs/tags/$tag.tar.gz" | sha256sum | cut -d' ' -f1)
    bin_sum=$(sha256sum "$out/$name.tar.gz" | cut -d' ' -f1)
    sed -i "s/^sha256sums=.*/sha256sums=('$src_sum')/" dist/arch/sysman/PKGBUILD
    sed -i "s/^sha256sums_x86_64=.*/sha256sums_x86_64=('$bin_sum')/" dist/arch/sysman-bin/PKGBUILD
    if [[ -n $(git status --porcelain dist/arch) ]]; then
        git commit -qm "Release $v: AUR checksums" dist/arch
        git push -q origin main
    fi

    echo "==> AUR: sysman and sysman-bin"
    just aur

    echo "==> the website"
    just site-deploy

    run=$(gh run list --workflow release.yml --event push --limit 10 --json databaseId,headBranch,url \
        --jq ".[] | select(.headBranch == \"$tag\") | .url" | head -1)
    echo
    echo "Published $v: $(gh release view "$tag" --json url --jq .url)"
    echo "GitHub is building the .deb, generic tarball and install.sh, and marks the release latest when they are up${run:+: $run}"

# Publish dist/arch (sysman, sysman-bin) to the AUR through throwaway clones in target/aur; bump pkgrel and commit first for a packaging-only fix
aur:
    #!/usr/bin/env bash
    set -euo pipefail
    ver={{ version }}
    fail() { echo "aur: $*" >&2; exit 1; }
    [[ -z $(git status --porcelain dist/arch) ]] || fail "commit dist/arch first"
    curl -fsIL --retry 6 --retry-delay 5 --retry-all-errors -o /dev/null \
        "https://github.com/mah3uz/sysman/releases/download/v$ver/sysman-$ver-arch-x86_64.tar.gz" ||
        fail "the v$ver GitHub release has no Arch tarball yet; run just publish $ver"
    for p in sysman sysman-bin; do
        src=dist/arch/$p
        grep -q "^pkgver=$ver$" "$src/PKGBUILD" || fail "$p: pkgver is not $ver"
        ! grep -q "^sha256sums.*SKIP" "$src/PKGBUILD" || fail "$p: no checksum yet"
        dir=target/aur/$p
        rm -rf "$dir"
        mkdir -p target/aur
        # a new package's AUR repo is empty, which git warns about
        git clone -q "ssh://aur@aur.archlinux.org/$p.git" "$dir" 2>/dev/null
        cp "$src/PKGBUILD" "$src/sysman.install" "$dir/"
        (cd "$dir" && makepkg --printsrcinfo > .SRCINFO)
        git -C "$dir" add PKGBUILD .SRCINFO sysman.install
        if git -C "$dir" diff --cached --quiet; then
            echo "$p: already up to date"
        else
            rel=$(sed -n 's/^pkgrel=//p' "$src/PKGBUILD")
            git -C "$dir" commit -qm "Update to $ver-$rel"
            # the AUR only accepts master, whatever init.defaultBranch named the clone's branch
            git -C "$dir" push -q origin HEAD:master
            echo "$p: pushed $ver-$rel"
        fi
        rm -rf "$dir"
    done

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
