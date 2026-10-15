#!/bin/sh
# Decide the next GitHub Release. Prints GitHub Actions outputs.
# Never deletes or force-pushes tags or main.
#
# env:
#   BEFORE    previous commit of the push (optional; all-zero = first push)
#   HEAD_SHA  current commit (default HEAD)
#   LABELS    PR labels, space or newline separated (optional)
#   COMMIT_MSG extra text to scan (optional)
#   FORCE     "true" forces a release even with no code changes (optional)
#
# Only changes that affect the shipped binary or release archives cause a
# release (allowlist, see ships()). A commit message containing [release]
# in the pushed range, or FORCE=true, forces one. A commit message containing
# [no release] skips the release (reason=no-release); a force wins over it.
set -euf

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

# Paths that end up in the binary or the release tarball.
# contrib/ is packed, except the CI-only release helpers.
ships() {
    case $1 in
        src/*|Cargo.toml|Cargo.lock|build.rs|config/*) return 0 ;;
        contrib/release-decide.sh|contrib/next-version.sh) return 1 ;;
        contrib/*) return 0 ;;
    esac
    return 1
}

# Succeeds when BEFORE..after is known and touches no shipped file.
no_code_changes() {
    before=$1
    after=$2
    if [ -z "$before" ] || [ "$before" = "0000000000000000000000000000000000000000" ]; then
        return 1
    fi
    if ! git cat-file -e "${before}^{commit}" 2>/dev/null; then
        return 1
    fi
    files=$(git diff --name-only "$before" "$after")
    for f in $files; do
        if ships "$f"; then
            return 1
        fi
    done
    return 0
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
    msgs=$(git log --format=%B "$BEFORE..$head_sha")
else
    msgs=$(git log -1 --format=%B "$head_sha")
fi
scan "$msgs"

force=no
case ${FORCE:-} in true|yes|1) force=yes ;; esac
if printf '%s\n' "$msgs" "${COMMIT_MSG:-}" | grep -qiF '[release]'; then
    force=yes
fi
norelease=no
if printf '%s\n' "$msgs" "${COMMIT_MSG:-}" | grep -qiF '[no release]'; then
    norelease=yes
fi

if [ -z "$latest" ]; then
    ver=0.1.1
    bump=patch
elif [ "$force" = no ] && [ "$norelease" = yes ]; then
    echo "skip=yes"
    echo "reason=no-release"
    echo "ver=${latest#v}"
    echo "tag=$latest"
    echo "bump="
    exit 0
elif [ "$force" = no ] && no_code_changes "${BEFORE:-}" "$head_sha"; then
    echo "skip=yes"
    echo "reason=no-code"
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
