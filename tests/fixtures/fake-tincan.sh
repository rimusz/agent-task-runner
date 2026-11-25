#!/bin/sh
# Test double for `tincan ask` / `tincan ping`.
# Sidecars next to this script (so parallel tests do not share env):
#   $0.log   argv record
#   $0.sleep seconds to sleep before exit
#   $0.fail  if present, exit 1
base=$0
log=${TINC_LOG:-$base.log}
printf '%s\n' "$0" "$@" >>"$log"
printf '---\n' >>"$log"
if [ -f "$base.sleep" ]; then
    sleep "$(cat "$base.sleep")"
fi
if [ -f "$base.fail" ]; then
    exit 1
fi
exit 0
