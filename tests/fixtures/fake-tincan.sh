#!/bin/sh
# Test double for `tincan ask` / `tincan ping`.
# Sidecars next to this script (so parallel tests do not share env):
#   $0.log   argv record
#   $0.sleep seconds to sleep before exit
#   $0.fail  if present, exit 1
base=$0
log=${TINC_LOG:-$base.log}
echo $$ >"$base.pid"
{
    printf '%s\n' "$0" "$@"
    printf '---\n'
    if [ -n "${ATR_TINCAN_SECRET+x}" ]; then
        printf 'HAS_ATR_TINCAN_SECRET\n'
    else
        printf 'NO_ATR_TINCAN_SECRET\n'
    fi
    if [ -n "${TINC_MARKER+x}" ]; then
        printf 'HAS_TINC_MARKER\n'
    else
        printf 'NO_TINC_MARKER\n'
    fi
} >>"$log"
if [ -f "$base.sleep" ]; then
    sleep "$(cat "$base.sleep")" &
    echo $! >"$base.sleeppid"
    wait
fi
if [ -f "$base.fail" ]; then
    exit 1
fi
exit 0
