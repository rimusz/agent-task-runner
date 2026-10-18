#!/usr/bin/env bash
# Read-only gh stub. Logs every invocation; write verbs fail.
set -euo pipefail

SELF_DIR=$(cd "$(dirname "$0")" && pwd)
if [[ -d "$SELF_DIR/../fake" ]]; then
  DIR=$(cd "$SELF_DIR/../fake" && pwd)
else
  DIR="${ATR_FAKE_DIR:-/tmp/atr-fake}"
fi
mkdir -p "$DIR"
printf '%s\n' "$*" >> "$DIR/gh-invocations.log"

write=0
prev=""
for a in "$@"; do
  ua=$(printf '%s' "$a" | tr '[:lower:]' '[:upper:]')
  if [[ "$prev" == "-X" || "$prev" == "--method" ]]; then
    case "$ua" in POST|PUT|PATCH|DELETE) write=1 ;; esac
  fi
  prev="$a"
done
cmd="${1:-}"
sub="${2:-}"
case "$cmd $sub" in
  "issue comment"|"issue edit"|"issue create"|"issue delete"|"issue close"|"issue reopen"|"issue develop"|"issue transfer"|"issue pin"|"issue unpin"|"pr create"|"pr comment"|"pr edit"|"pr merge"|"pr close"|"pr ready"|"pr review"|"label create"|"label delete"|"label edit"|"label clone")
    write=1 ;;
esac
if [[ "$cmd $sub" == "pr create" && -f "$DIR/gh-allow-pr" ]]; then
  printf '%s\n' "$@" >> "$DIR/gh-pr.log"
  echo "https://github.com/example/repo/pull/7"
  exit 0
fi
if [[ "$write" -eq 1 ]]; then
  echo "$*" >> "$DIR/gh-writes.log"
  echo "fake-gh: refusing write: $*" >&2
  exit 2
fi

if [[ "$cmd" == "issue" && "$sub" == "list" ]]; then
  if [[ -f "$DIR/gh-issues.json" ]]; then
    cat "$DIR/gh-issues.json"
  else
    echo '[]'
  fi
  exit 0
fi
if [[ "$cmd" == "api" ]]; then
  if [[ -f "$DIR/gh-comments.json" ]]; then
    cat "$DIR/gh-comments.json"
  else
    echo '[]'
  fi
  exit 0
fi
if [[ "$cmd" == "repo" && "$sub" == "clone" ]]; then
  dest="${4:-}"
  src_repo="${3:-}"
  if [[ -z "$dest" ]]; then
    dest=$(basename "${src_repo}")
  fi
  mkdir -p "$(dirname "$dest")"
  clone_src="${ATR_SHADOW_CLONE_SRC:-}"
  if [[ -z "$clone_src" && -f "$DIR/clone-src" ]]; then
    clone_src=$(cat "$DIR/clone-src")
  fi
  if [[ -n "$clone_src" && -d "$clone_src" ]]; then
    git clone -- "$clone_src" "$dest"
  else
    git init -b main "$dest"
    git -C "$dest" config user.email "atr@example.test"
    git -C "$dest" config user.name "atr"
    git -C "$dest" config commit.gpgsign false
    echo "shadow clone" > "$dest/README.md"
    git -C "$dest" add README.md
    git -C "$dest" commit -m init
  fi
  exit 0
fi
echo '[]'
exit 0
