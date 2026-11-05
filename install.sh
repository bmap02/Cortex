#!/bin/sh
# Cortex installer for macOS and Linux.
#   curl -fsSL https://raw.githubusercontent.com/bmap02/Cortex/main/install.sh | sh
# Options (environment variables):
#   CORTEX_REPO         GitHub repo to download from (default below)
#   CORTEX_VERSION      a tag like v0.1.0 (default: latest release)
#   CORTEX_INSTALL_DIR  where to put the binary (default: ~/.local/bin)
#   CORTEX_RELEASES_URL download from a mirror instead of GitHub Releases
set -eu

REPO="${CORTEX_REPO:-bmap02/Cortex}"
VERSION="${CORTEX_VERSION:-latest}"
DIR="${CORTEX_INSTALL_DIR:-$HOME/.local/bin}"
BASE="${CORTEX_RELEASES_URL:-https://github.com/$REPO/releases}"

case "$(uname -s)" in
  Linux)  os=unknown-linux-gnu ;;
  Darwin) os=apple-darwin ;;
  *) echo "Unsupported OS: $(uname -s). On Windows, use install.ps1." >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64|amd64)  arch=x86_64 ;;
  arm64|aarch64) arch=aarch64 ;;
  *) echo "Unsupported CPU: $(uname -m). Build from source: cargo install --git https://github.com/$REPO" >&2; exit 1 ;;
esac
target="$arch-$os"

if [ "$VERSION" = latest ]; then
  url="$BASE/latest/download/cortex-$target.tar.gz"
else
  url="$BASE/download/$VERSION/cortex-$target.tar.gz"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
echo "Downloading $url"
if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
else
  echo "Need curl or wget." >&2; exit 1
fi
if ! fetch "$url" "$tmp/cortex.tar.gz"; then
  echo >&2
  echo "Couldn't download Cortex for $target." >&2
  echo "  - No release published yet? Check https://github.com/$REPO/releases" >&2
  echo "  - Pinned a version? Make sure CORTEX_VERSION=$VERSION exists." >&2
  echo "  - Or build from source instead: cargo install --git https://github.com/$REPO" >&2
  exit 1
fi

tar -xzf "$tmp/cortex.tar.gz" -C "$tmp"
mkdir -p "$DIR"
mv "$tmp/cortex/cortex" "$DIR/cortex"
chmod +x "$DIR/cortex"
# macOS: clear the quarantine flag so Gatekeeper doesn't block an unsigned download
if [ "$(uname -s)" = Darwin ]; then xattr -d com.apple.quarantine "$DIR/cortex" 2>/dev/null || true; fi

echo "Installed $("$DIR/cortex" --version) to $DIR/cortex"
case ":$PATH:" in
  *":$DIR:"*) ;;
  *)
    echo
    if [ "$DIR" = "$HOME/.local/bin" ] && grep -qs '.local/bin' "$HOME/.profile" "$HOME/.bash_profile" "$HOME/.zprofile"; then
      # Ubuntu, Debian, Fedora etc. add ~/.local/bin automatically once it exists
      echo "Open a new terminal and 'cortex' will be on your PATH."
    else
      rc="$HOME/.$(basename "${SHELL:-sh}")rc"
      echo "Add $DIR to your PATH:"
      echo "  echo 'export PATH=\"$DIR:\$PATH\"' >> $rc"
      echo "then open a new terminal."
    fi ;;
esac
echo
echo "Next: run 'cortex setup' to connect your model server."
