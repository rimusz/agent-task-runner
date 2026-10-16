#!/bin/sh
# atr installer: curl -fsSL https://raw.githubusercontent.com/rimusz/agent-task-runner/main/install.sh | sh
# Env: ATR_VERSION (default: latest), ATR_BIN_DIR (default: ~/.local/bin)
set -eu
REPO=rimusz/agent-task-runner
BIN_DIR="${ATR_BIN_DIR:-$HOME/.local/bin}"
os=$(uname -s); arch=$(uname -m)
case "$os/$arch" in
  Darwin/arm64)          platform=macos-arm64 ;;
  Darwin/*)              echo "No Intel Mac build. Use: cargo install --git https://github.com/$REPO" >&2; exit 1 ;;
  Linux/x86_64|Linux/amd64)
    if ldd --version 2>&1 | grep -qi musl; then platform=linux-x86_64-musl; else platform=linux-x86_64; fi ;;
  Linux/aarch64|Linux/arm64) platform=linux-arm64 ;;
  *) echo "Unsupported platform: $os/$arch" >&2; exit 1 ;;
esac
ver="${ATR_VERSION:-}"
if [ -z "$ver" ]; then
  ver=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | sed -n 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/p' | head -n1)
fi
[ -n "$ver" ] || { echo "Could not find the latest release." >&2; exit 1; }
file="atr-$ver-$platform.tar.gz"
url="https://github.com/$REPO/releases/download/v$ver/$file"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
echo "Downloading $file"
curl -fsSL -o "$tmp/$file" "$url"
curl -fsSL -o "$tmp/$file.sha256" "$url.sha256"
want=$(awk '{print $1}' "$tmp/$file.sha256")
if command -v sha256sum >/dev/null 2>&1; then got=$(sha256sum "$tmp/$file" | awk '{print $1}'); else got=$(shasum -a 256 "$tmp/$file" | awk '{print $1}'); fi
[ "$want" = "$got" ] || { echo "Checksum mismatch for $file" >&2; exit 1; }
tar -xzf "$tmp/$file" -C "$tmp"
mkdir -p "$BIN_DIR"
install -m 755 "$tmp/atr" "$BIN_DIR/atr"
echo "Installed atr $ver to $BIN_DIR/atr"
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *)
    case "${SHELL##*/}" in
      zsh) rc="~/.zshenv" ;;
      bash) rc="~/.bashrc" ;;
      *) rc="~/.profile" ;;
    esac
    echo "WARNING: $BIN_DIR is not on your PATH, so \`atr\` will be 'command not found'. Fix:"
    echo "  echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> $rc && exec \$SHELL -l"
    ;;
esac
echo "Next: atr init"
