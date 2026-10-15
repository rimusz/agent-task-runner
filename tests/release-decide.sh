#!/bin/sh
# Checks contrib/release-decide.sh (on-demand releases only).
# Run: sh tests/release-decide.sh
set -eu

script=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)/contrib/release-decide.sh
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cd "$tmp"
git init -q
git config user.email t@t
git config user.name t
mkdir -p src
echo 'fn main() {}' > src/main.rs
git add -A && git commit -qm init

fails=0
# check NAME WANT_SKIP WANT_TAG [VAR=VALUE...]
check() {
    name=$1 want=$2 wtag=$3
    shift 3
    if ! out=$(env -u TAG -u VERSION -u BUMP "$@" sh "$script" 2>&1); then
        out="skip=error"
    fi
    got=$(printf '%s\n' "$out" | sed -n 's/^skip=//p')
    gtag=$(printf '%s\n' "$out" | sed -n 's/^tag=//p')
    if [ "$got" = "$want" ] && { [ "$want" = error ] || [ "$gtag" = "$wtag" ]; }; then
        echo "ok   $name (skip=$got tag=$gtag)"
    else
        echo "FAIL $name: want skip=$want tag=$wtag got skip=$got tag=$gtag"
        printf '%s\n' "$out"
        fails=$((fails + 1))
    fi
}
change() { echo x >> "$1"; git add -A; git commit -qm "${MSG:-change}"; }

check first-release no v0.1.1
git tag v0.1.1
check head-already-tagged yes v0.1.1
change README.md
check docs-only-still-releases no v0.1.2
MSG='x [no release]' change src/main.rs
check marker-ignored no v0.1.2
check bump-minor no v0.2.0 BUMP=minor
check bump-major no v1.0.0 BUMP=major
check bump-invalid error "" BUMP=huge
check version-input no v0.3.0 VERSION=0.3.0
check version-input-v no v0.3.0 VERSION=v0.3.0
check version-exists yes v0.1.1 VERSION=0.1.1
check version-invalid error "" VERSION=1.2
check tag-push no v0.4.0 TAG=v0.4.0
check tag-push-ref no v0.4.0 TAG=refs/tags/v0.4.0
check tag-invalid error "" TAG=vfoo

if [ "$fails" -ne 0 ]; then
    echo "$fails case(s) failed"
    exit 1
fi
echo "all release-decide cases passed"
