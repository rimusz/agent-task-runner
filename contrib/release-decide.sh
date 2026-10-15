#!/bin/sh
# Decide the next GitHub Release. Prints GitHub Actions outputs.
# Never deletes or force-pushes tags or main.
#
# env:
#   BEFORE    previous commit of the push (optional; all-zero = first push)
#   HEAD_SHA  current commit (default HEAD)
#   LABELS    PR labels, space or newline separated (optional)
#   COMMIT_MSG extra text to scan (optional)
set -eu

here=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
head_sha=${HEAD_SHA:-HEAD}
head_sha=$(git rev-parse "$head_sha")

if tag=$(git describe --tags --exact-match --match 'v[0-9]*' "$head_sha" 2>/dev/null); then
    echo "skip=yes"
    echo "reason=tagged"
    echo "ver=${tag#v}"
    echo "tag=$tag"
    echo "bump="
    exit 0
fi

latest=$(git tag -l 'v[0-9]*' --sort=-v:refname | head -n 1 || true)

is_docs_only() {
    before=$1
    after=$2
    if [ -z "$before" ] || [ "$before" = "0000000000000000000000000000000000000000" ]; then
        return 1
    fi
    if ! git cat-file -e "${before}^{commit}" 2>/dev/null; then
        return 1
    fi
    files=$(git diff --name-only "$before" "$after")
    [ -n "$files" ] || return 1
    leftover=$(printf '%s\n' "$files" | grep -vE '(^docs/)|(\.md$)|(^LICENSE)|(^\.github/ISSUE_TEMPLATE/)' || true)
    [ -z "$leftover" ]
}

bump=patch
scan() {
    if printf '%s\n' "$1" | grep -qiE 'release:major'; then
        bump=major
    elif [ "$bump" != major ] && printf '%s\n' "$1" | grep -qiE 'release:minor'; then
        bump=minor
    fi
}

scan "${LABELS:-}"
scan "${COMMIT_MSG:-}"

if [ -n "${BEFORE:-}" ] && [ "$BEFORE" != "0000000000000000000000000000000000000000" ] \
    && git cat-file -e "${BEFORE}^{commit}" 2>/dev/null; then
    scan "$(git log --format=%B "$BEFORE..$head_sha")"
else
    scan "$(git log -1 --format=%B "$head_sha")"
fi

if [ -z "$latest" ]; then
    ver=0.1.1
    bump=patch
elif is_docs_only "${BEFORE:-}" "$head_sha"; then
    echo "skip=yes"
    echo "reason=docs"
    echo "ver=${latest#v}"
    echo "tag=$latest"
    echo "bump="
    exit 0
else
    ver=$(sh "$here/next-version.sh" "$latest" "$bump")
fi

echo "skip=no"
echo "reason=release"
echo "ver=$ver"
echo "tag=v$ver"
echo "bump=$bump"
