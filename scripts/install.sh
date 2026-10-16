#!/bin/sh
# Put the herdr-bar binary at bin/herdr-bar for this checkout.
#
# Herdr runs this as the plugin's [[build]] step on `herdr plugin install` and
# `herdr plugin update` (not on `plugin link`: run it yourself for a local
# checkout). It downloads the prebuilt binary of the manifest's version from
# the GitHub release, checks it against the release's SHA256SUMS, and falls
# back to `cargo build --release` when there is no binary for this machine or
# the download or the check fails.
#
#   HERDR_BAR_BUILD=source     skip the download and build with cargo
#   HERDR_BAR_DOWNLOAD_BASE=   release download URL to use instead of GitHub's
#                              (the directory holding the assets)
set -u

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd) || exit 1
cd "$ROOT" || exit 1
BIN_DIR="$ROOT/bin"
TARGET="$BIN_DIR/herdr-bar"
REPO_URL="https://github.com/jeffarese/herdr-bar"

say() { printf 'herdr-bar: %s\n' "$*" >&2; }

version=$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' herdr-plugin.toml | head -n 1)
if [ -z "$version" ]; then
    say "cannot read the version from herdr-plugin.toml"
    exit 1
fi

asset_name() {
    case "$(uname -s)" in
        Linux) os=linux ;;
        Darwin) os=macos ;;
        *) return 1 ;;
    esac
    arch=$(uname -m)
    # A shell under Rosetta reports x86_64 on Apple silicon; take the native build.
    if [ "$os" = macos ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null)" = 1 ]; then
        arch=arm64
    fi
    case "$arch" in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) return 1 ;;
    esac
    printf 'herdr-bar-%s-%s\n' "$os" "$arch"
}

fetch() { # url destination
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 2 --connect-timeout 15 -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -T 30 -O "$2" "$1"
    else
        say "neither curl nor wget is available"
        return 1
    fi
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        say "neither sha256sum nor shasum is available"
        return 1
    fi
}

# Move a file into bin/ in one rename, so a running watcher never sees half a binary.
install_binary() { # source
    mkdir -p "$BIN_DIR" || return 1
    cp "$1" "$TARGET.tmp.$$" && chmod 755 "$TARGET.tmp.$$" && mv -f "$TARGET.tmp.$$" "$TARGET" || {
        rm -f "$TARGET.tmp.$$"
        return 1
    }
}

download() {
    asset=$(asset_name) || {
        say "no prebuilt binary for $(uname -s) $(uname -m)"
        return 1
    }
    base="${HERDR_BAR_DOWNLOAD_BASE:-$REPO_URL/releases/download/v$version}"
    tmp=$(mktemp -d 2>/dev/null || mktemp -d -t herdr-bar) || return 1
    say "downloading $asset $version"
    ok=1
    if fetch "$base/$asset" "$tmp/$asset" && fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS"; then
        expected=$(awk -v name="$asset" '$2 == name || $2 == "*" name {print $1; exit}' "$tmp/SHA256SUMS")
        actual=$(sha256 "$tmp/$asset")
        if [ -z "$expected" ]; then
            say "SHA256SUMS has no entry for $asset"
        elif [ "$expected" != "$actual" ]; then
            say "checksum mismatch for $asset (expected $expected, got $actual)"
        elif ! chmod 755 "$tmp/$asset" || ! "$tmp/$asset" --version >/dev/null 2>&1; then
            say "the downloaded $asset does not run on this machine"
        elif install_binary "$tmp/$asset"; then
            ok=0
        fi
    else
        say "download failed"
    fi
    rm -rf "$tmp"
    return $ok
}

build_from_source() {
    if ! command -v cargo >/dev/null 2>&1; then
        say "cargo is not installed"
        return 1
    fi
    say "building from source with cargo"
    locked=
    [ -f Cargo.lock ] && locked=--locked
    cargo build --release $locked && install_binary "$ROOT/target/release/herdr-bar"
}

if [ "${HERDR_BAR_BUILD:-}" != source ] && download; then
    say "installed $TARGET"
    exit 0
fi
if build_from_source; then
    say "installed $TARGET"
    exit 0
fi
say "could not install herdr-bar $version: no prebuilt binary could be downloaded"
say "and building from source failed. Install Rust (https://rustup.rs) and run"
say "  sh scripts/install.sh"
say "in $ROOT, or reinstall the plugin."
exit 1
