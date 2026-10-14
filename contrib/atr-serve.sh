#!/bin/sh
# LaunchAgent / systemd entrypoint: load secrets from shadow.env, then exec atr serve.
# Tokens never belong in the plist, unit file, or config.toml.
# `atr init` / `atr service install` replace __ATR_BIN__ with the real binary.
set -eu
env_file="${HOME}/.config/atr/shadow.env"
if [ -f "$env_file" ]; then
  set -a
  # shellcheck disable=SC1090
  . "$env_file"
  set +a
fi
if [ -n "${ATR_BIN:-}" ] && [ -x "${ATR_BIN}" ]; then
  exec "$ATR_BIN" serve "$@"
fi
if [ -x "__ATR_BIN__" ]; then
  exec "__ATR_BIN__" serve "$@"
fi
exec atr serve "$@"
