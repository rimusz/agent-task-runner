#!/bin/sh
# atr installer: curl -fsSL https://raw.githubusercontent.com/rimusz/agent-task-runner/main/install.sh | sh
# Env: ATR_VERSION (default: latest), ATR_BIN_DIR (default: ~/.local/bin),
#      ATR_NO_MODIFY_PATH=1 (do not edit shell files to add BIN_DIR to PATH)
set -eu

# ensure_path DIR: when DIR is not on PATH, add a marked, idempotent line to
# the user's shell startup file. Opt out with ATR_NO_MODIFY_PATH=1.
ensure_path() {
  dir=$1
  case ":${PATH:-}:" in *":$dir:"*) return 0 ;; esac
  line="export PATH=\"$dir:\$PATH\" # added by atr installer"
  if [ "${ATR_NO_MODIFY_PATH:-}" = 1 ]; then
    echo "$dir is not on PATH (ATR_NO_MODIFY_PATH=1, not changed). Add to your shell file:"
    echo "  $line"
    return 0
  fi
  sh_name=${SHELL##*/}
  if [ "$sh_name" = fish ]; then
    if command -v fish >/dev/null 2>&1; then
      fish -c "fish_add_path -g -p '$dir'"
      echo "Added $dir to PATH with fish_add_path. Undo: set -e fish_user_paths[(contains -i '$dir' \$fish_user_paths)]"
      return 0
    fi
  fi
  case "$sh_name/${ATR_OS:-$(uname -s)}" in
    zsh/*) rc="$HOME/.zshenv" ;;
    bash/Darwin) rc="$HOME/.bash_profile" ;;
    bash/*) rc="$HOME/.bashrc" ;;
    *) rc="$HOME/.profile" ;;
  esac
  if [ -f "$rc" ] && grep -qF "# added by atr installer" "$rc"; then
    echo "$rc already has the atr PATH line; open a new shell."
    return 0
  fi
  printf '\n%s\n' "$line" >> "$rc"
  echo "Added $dir to PATH in $rc. Open a new shell (or: . \"$rc\")."
  echo "Undo: delete the line ending in '# added by atr installer' from $rc."
}
[ "${ATR_INSTALL_LIB:-}" = 1 ] && return 0
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
ensure_path "$BIN_DIR"
echo "Next: atr init"
