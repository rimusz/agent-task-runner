#!/usr/bin/env bash
# Fails on the first invocation, then passes. Count file is $1 or ATR_FAKE_DIR.
set -euo pipefail
FILE="${1:-}"
if [[ -z "$FILE" ]]; then
  FILE="${ATR_FAKE_DIR:-.}/verify.count"
fi
mkdir -p "$(dirname "$FILE")"
n=0
if [[ -f "$FILE" ]]; then
  n=$(cat "$FILE")
fi
n=$((n + 1))
printf '%s\n' "$n" > "$FILE"
if [[ "$n" -eq 1 ]]; then
  echo "verify failed: unexpected any in src/app.ts" >&2
  exit 1
fi
echo "verify ok"
exit 0
