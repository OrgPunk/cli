#!/bin/sh
# Installs opk: the release binary for this OS and CPU, checked against the release's
# SHA256SUMS.
#
#   curl -fsSL https://raw.githubusercontent.com/OrgPunk/cli/main/install.sh | sh
#
# Environment:
#   ORGPUNK_VERSION       release tag to install, e.g. v0.2.0 (default: the latest release)
#   ORGPUNK_INSTALL_DIR   where to put the binary (default: ~/.local/bin)
#   ORGPUNK_REPO          GitHub repository to download from (default: OrgPunk/cli)
#   ORGPUNK_DOWNLOAD_URL  base URL of the release files instead of GitHub (mirrors, tests)
set -eu

REPO="${ORGPUNK_REPO:-OrgPunk/cli}"
VERSION="${ORGPUNK_VERSION:-latest}"
INSTALL_DIR="${ORGPUNK_INSTALL_DIR:-$HOME/.local/bin}"

fail() {
    echo "opk install: $*" >&2
    exit 1
}

case "$(uname -s)" in
    Linux) os=unknown-linux-musl ;;
    Darwin) os=apple-darwin ;;
    *) fail "unsupported OS $(uname -s); on Windows download opk-x86_64-pc-windows-msvc.zip from https://github.com/$REPO/releases" ;;
esac
case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    arm64 | aarch64) arch=aarch64 ;;
    *) fail "unsupported CPU $(uname -m)" ;;
esac
target="$arch-$os"
asset="opk-$target.tar.gz"

if [ -n "${ORGPUNK_DOWNLOAD_URL:-}" ]; then
    base="$ORGPUNK_DOWNLOAD_URL"
elif [ "$VERSION" = latest ]; then
    base="https://github.com/$REPO/releases/latest/download"
else
    base="https://github.com/$REPO/releases/download/$VERSION"
fi

download() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
        wget -q "$1" -O "$2"
    else
        fail "needs curl or wget"
    fi
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    else
        fail "needs sha256sum or shasum to check the download"
    fi
}

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Downloading opk ($VERSION, $target)…"
download "$base/$asset" "$tmp/$asset" || fail "cannot download $base/$asset"
download "$base/SHA256SUMS" "$tmp/SHA256SUMS" || fail "cannot download $base/SHA256SUMS"

expected="$(grep " $asset\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)"
[ -n "$expected" ] || fail "$asset is not in SHA256SUMS"
[ "$(sha256 "$tmp/$asset")" = "$expected" ] || fail "checksum mismatch for $asset"

tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$INSTALL_DIR"
cp "$tmp/opk" "$INSTALL_DIR/opk"
chmod 755 "$INSTALL_DIR/opk"

echo "Installed $("$INSTALL_DIR/opk" --version) to $INSTALL_DIR/opk"
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        echo
        echo "$INSTALL_DIR is not on your PATH. Add it, e.g. in ~/.bashrc or ~/.zshrc:"
        echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
        ;;
esac

case "${SHELL##*/}" in
    zsh) rc="~/.zshrc"; line="source <(opk completion zsh)" ;;
    fish) rc="~/.config/fish/config.fish"; line="opk completion fish | source" ;;
    *) rc="~/.bashrc"; line="source <(opk completion bash)" ;;
esac
echo
echo "Tab completion: add this line to $rc"
echo "  $line"
