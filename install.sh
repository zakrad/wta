#!/usr/bin/env bash
# wta installer — downloads the latest prebuilt binary for your platform.
#   curl -fsSL https://raw.githubusercontent.com/zakrad/wta/main/install.sh | bash
# Override the install dir with WTA_BINDIR=/usr/local/bin.
set -euo pipefail

REPO="zakrad/wta"
BINDIR="${WTA_BINDIR:-$HOME/.local/bin}"

os="$(uname -s)"
arch="$(uname -m)"
case "$os-$arch" in
  Darwin-arm64)   target="aarch64-apple-darwin" ;;
  Darwin-x86_64)  target="x86_64-apple-darwin" ;;
  Linux-x86_64)   target="x86_64-unknown-linux-gnu" ;;
  *)
    echo "wta: no prebuilt binary for $os-$arch."
    echo "build from source instead:  cargo install --git https://github.com/$REPO"
    exit 1 ;;
esac

tag="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
        | grep -m1 '"tag_name"' | cut -d'"' -f4)"
[ -n "$tag" ] || { echo "wta: no published release yet — try: cargo install --git https://github.com/$REPO"; exit 1; }

url="https://github.com/$REPO/releases/download/$tag/wta-$target.tar.gz"
echo "wta: downloading $tag for $target ..."
mkdir -p "$BINDIR"
curl -fsSL "$url" | tar -xz -C "$BINDIR"
chmod +x "$BINDIR/wta"
echo "wta: installed to $BINDIR/wta"

command -v tmux >/dev/null 2>&1 || echo "wta: also install tmux (e.g. 'brew install tmux') — wta needs it."
case ":$PATH:" in
  *":$BINDIR:"*) ;;
  *) echo "wta: add $BINDIR to your PATH  (e.g. echo 'export PATH=\"$BINDIR:\$PATH\"' >> ~/.zshrc)" ;;
esac
