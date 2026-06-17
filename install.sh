#!/bin/sh
# chatgpt-use installer — downloads a prebuilt binary from GitHub Releases.
# No npm, no token. Usage:
#   curl -fsSL https://raw.githubusercontent.com/leeguooooo/chatgpt-use/main/install.sh | sh
set -eu

REPO="leeguooooo/chatgpt-use"
BIN="chatgpt-use"
INSTALL_DIR="${CHATGPT_USE_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }

# --- detect platform -> release asset target triple ---------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Darwin) case "$arch" in
            arm64|aarch64) target="aarch64-apple-darwin" ;;
            x86_64)        target="x86_64-apple-darwin" ;;
            *) die "unsupported macOS arch: $arch" ;;
          esac ;;
  Linux)  case "$arch" in
            x86_64)        target="x86_64-unknown-linux-gnu" ;;
            aarch64|arm64) target="aarch64-unknown-linux-gnu" ;;
            *) die "unsupported Linux arch: $arch" ;;
          esac ;;
  *) die "unsupported OS: $os (build from source: cargo install --git https://github.com/$REPO)" ;;
esac

# --- resolve latest release tag -----------------------------------------------
tag="${CHATGPT_USE_VERSION:-}"
if [ -z "$tag" ]; then
  tag="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
          | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)"
fi
[ -n "$tag" ] || die "could not resolve a release tag (set CHATGPT_USE_VERSION, or build from source)"

asset="$BIN-$target.tar.gz"
url="https://github.com/$REPO/releases/download/$tag/$asset"

say "==> chatgpt-use $tag ($target)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

say "==> downloading $url"
curl -fsSL "$url" -o "$tmp/$asset" || die "download failed — does release $tag ship $asset?"

# verify checksum if published alongside
if curl -fsSL "$url.sha256" -o "$tmp/$asset.sha256" 2>/dev/null; then
  ( cd "$tmp" && (sha256sum -c "$asset.sha256" 2>/dev/null \
                  || shasum -a 256 -c "$asset.sha256")) || die "checksum mismatch"
fi

tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$INSTALL_DIR"
install -m 0755 "$tmp/$BIN" "$INSTALL_DIR/$BIN" 2>/dev/null || {
  cp "$tmp/$BIN" "$INSTALL_DIR/$BIN" && chmod 0755 "$INSTALL_DIR/$BIN"; }

say "==> installed $INSTALL_DIR/$BIN"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) : ;;
  *) say "note: add $INSTALL_DIR to your PATH:  export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac

# chatgpt-use requires chrome-use on PATH
if ! command -v chrome-use >/dev/null 2>&1; then
  say ""
  say "chatgpt-use requires 'chrome-use'. Install it with:"
  say "  curl -fsSL https://raw.githubusercontent.com/leeguooooo/chrome-use/main/install.sh | sh"
fi

say "==> done. Try:  $BIN --help"
