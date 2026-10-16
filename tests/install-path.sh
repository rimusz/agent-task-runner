#!/bin/sh
# shellcheck disable=SC2016,SC2030
# Cases for install.sh ensure_path. Run: sh tests/install-path.sh
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
fail=0
check() { if eval "$2"; then echo "ok   $1"; else echo "FAIL $1"; fail=1; fi; }

run() { # shell os -> prints rc file contents dir
  t=$(mktemp -d)
  (
    HOME=$t; SHELL=/bin/$1; ATR_OS=$2; PATH=/usr/bin:/bin; ATR_INSTALL_LIB=1
    export HOME SHELL ATR_OS PATH ATR_INSTALL_LIB
    # shellcheck disable=SC1091
    . "$root/install.sh"
    ensure_path "$t/bin" >/dev/null
    ensure_path "$t/bin" >/dev/null
  )
  echo "$t"
}

t=$(run zsh Darwin)
check "zsh -> .zshenv" '[ "$(grep -c "added by atr installer" "$t/.zshenv")" = 1 ]'
t=$(run bash Darwin)
check "bash macOS -> .bash_profile" '[ "$(grep -c "added by atr installer" "$t/.bash_profile")" = 1 ]'
t=$(run bash Linux)
check "bash linux -> .bashrc" '[ "$(grep -c "added by atr installer" "$t/.bashrc")" = 1 ]'
t=$(run dash Linux)
check "other -> .profile" '[ "$(grep -c "added by atr installer" "$t/.profile")" = 1 ]'
check "line exports dir" 'grep -qF "export PATH=\"$t/bin:\$PATH\"" "$t/.profile"'

t=$(mktemp -d)
(
  HOME=$t; SHELL=/bin/zsh; ATR_OS=Darwin; PATH=/usr/bin:/bin; ATR_INSTALL_LIB=1; ATR_NO_MODIFY_PATH=1
  export HOME SHELL ATR_OS PATH ATR_INSTALL_LIB ATR_NO_MODIFY_PATH
  # shellcheck disable=SC1091
  . "$root/install.sh"
  ensure_path "$t/bin" >/dev/null
)
check "opt-out leaves files alone" '[ ! -e "$t/.zshenv" ]'

t=$(mktemp -d)
(
  HOME=$t; SHELL=/bin/zsh; ATR_OS=Darwin; PATH="/usr/bin:$t/bin"; ATR_INSTALL_LIB=1
  export HOME SHELL ATR_OS PATH ATR_INSTALL_LIB
  # shellcheck disable=SC1091
  . "$root/install.sh"
  ensure_path "$t/bin" >/dev/null
)
check "already on PATH: no change" '[ ! -e "$t/.zshenv" ]'

exit $fail
