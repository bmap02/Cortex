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
BASE="${BASE%/}" # a mirror given with a trailing slash would make //latest/download

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
  rel="$BASE/latest/download"
else
  rel="$BASE/download/$VERSION"
fi
file="cortex-$target.tar.gz"
url="$rel/$file"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
  # HTTP status of a URL after redirects (empty if the server can't be reached)
  http_status() { curl -sIL -o /dev/null -w '%{http_code}' "$1" 2>/dev/null || true; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
  http_status() { wget -S --spider "$1" 2>&1 | awk '/^  HTTP\//{c=$2} END{print c}' || true; }
else
  echo "Need curl or wget." >&2; exit 1
fi

echo "Downloading $url"
if ! fetch "$url" "$tmp/$file"; then
  echo >&2
  echo "Couldn't download Cortex for $target." >&2
  if [ "$(http_status "$url")" = 404 ]; then
    if [ -n "${CORTEX_VERSION:-}" ]; then
      echo "  - Make sure the release $VERSION exists and has a download for $target: $BASE" >&2
    else
      echo "  - No release published yet, or none for $target? Check $BASE" >&2
    fi
  else
    echo "  - Check your internet connection, proxy or firewall, then try again." >&2
  fi
  echo "  - Or build from source instead: cargo install --git https://github.com/$REPO" >&2
  exit 1
fi

# Check the download against the release's SHA256SUMS.txt
sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1
  fi
}
expected=""
if fetch "$rel/SHA256SUMS.txt" "$tmp/SHA256SUMS.txt" 2>/dev/null; then
  expected="$(awk -v f="$file" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS.txt")"
fi
actual="$(sha256 "$tmp/$file" || true)"
# every release publishes one: without it the download can't be told apart from a swapped one
if [ -z "$expected" ]; then
  echo "Couldn't get the checksum for $file (SHA256SUMS.txt), so the download can't be verified." >&2
  echo "Cortex was not installed; try again later." >&2
  exit 1
elif [ -z "$actual" ]; then
  echo "Warning: sha256sum/shasum not found; skipping checksum verification." >&2
elif [ "$expected" != "$actual" ]; then
  echo "Checksum mismatch for $file (expected $expected, got $actual)." >&2
  echo "The download may be corrupted or tampered with. Cortex was not installed." >&2
  exit 1
else
  echo "Checksum OK"
fi

tar -xzf "$tmp/$file" -C "$tmp"
mkdir -p "$DIR"
mv "$tmp/cortex/cortex" "$DIR/cortex"
chmod +x "$DIR/cortex"
# macOS: clear the quarantine flag so Gatekeeper doesn't block an unsigned download
if [ "$(uname -s)" = Darwin ]; then xattr -d com.apple.quarantine "$DIR/cortex" 2>/dev/null || true; fi

if ! v="$("$DIR/cortex" --version)"; then
  echo "cortex was installed to $DIR/cortex but failed to run." >&2
  exit 1
fi
echo "Installed $v to $DIR/cortex"
case ":$PATH:" in
  *":$DIR:"*) ;;
  *)
    echo
    echo "$DIR isn't on your PATH yet. To use cortex in this terminal, run:"
    echo "  export PATH=\"$DIR:\$PATH\""
    if [ "$DIR" = "$HOME/.local/bin" ] && grep -qs '\.local/bin' "$HOME/.profile" "$HOME/.bash_profile" "$HOME/.zprofile"; then
      # e.g. Ubuntu/Debian: ~/.profile adds ~/.local/bin, but only if it existed at login
      echo "Your login profile adds it automatically from your next login (log out and back in)."
    else
      rc="$HOME/.$(basename "${SHELL:-sh}")rc"
      echo "To make it permanent:"
      echo "  echo 'export PATH=\"$DIR:\$PATH\"' >> $rc"
    fi ;;
esac
echo
echo "Next: run 'cortex setup' to connect your model server."
