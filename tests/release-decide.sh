#!/bin/sh
# Checks contrib/release-decide.sh: only shipped-file changes release.
# Run: sh tests/release-decide.sh
set -eu

script=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)/contrib/release-decide.sh
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cd "$tmp"
git init -q
git config user.email t@t
git config user.name t
mkdir -p src docs tests .github/workflows config contrib
echo 'fn main() {}' > src/main.rs
git add -A && git commit -qm init && git tag v0.1.1

fails=0
# check NAME EXPECTED_SKIP [FORCE] -- commits made since the tag are judged
check() {
    name=$1 want=$2 force=${3:-}
    base=$(git rev-parse v0.1.1)
    out=$(BEFORE=$base FORCE=$force sh "$script")
    got=$(printf '%s\n' "$out" | sed -n 's/^skip=//p')
    if [ "$got" = "$want" ]; then
        echo "ok   $name (skip=$got)"
    else
        echo "FAIL $name: want skip=$want got skip=$got"
        printf '%s\n' "$out"
        fails=$((fails + 1))
    fi
    git reset -q --hard v0.1.1
}
change() { for f in "$@"; do mkdir -p "$(dirname "$f")"; echo x >> "$f"; done; git add -A; git commit -qm "${MSG:-change}"; }

change README.md docs/a.md;          check docs-only yes
change tests/t.rs;                   check tests-only yes
change .github/workflows/ci.yml;     check workflow-only yes
change install.sh LICENSE-MIT;       check install-and-license yes
change contrib/release-decide.sh;    check release-helper-only yes
change src/main.rs;                  check src no
change Cargo.lock;                   check cargo-lock no
change Cargo.toml;                   check cargo-toml no
change config/example.toml;          check config no
change contrib/atr.service;          check contrib-packed no
change README.md src/main.rs;        check mixed no
change README.md; change docs/b.md;  check docs-two-commits yes
change README.md;                    check force-env no true
MSG='docs tweak [release]' change README.md; check force-commit-msg no

if [ "$fails" -ne 0 ]; then
    echo "$fails case(s) failed"
    exit 1
fi
echo "all release-decide cases passed"
