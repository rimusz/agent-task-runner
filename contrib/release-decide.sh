#!/bin/sh
# Decide the version of an on-demand GitHub Release. Prints GitHub Actions
# outputs (skip, reason, ver, tag, bump). Releases are never automatic: the
# release workflow only runs on workflow_dispatch or a pushed v*.*.* tag.
# Never deletes or force-pushes tags or main.
#
# env:
#   TAG       pushed tag (e.g. v0.2.0) when triggered by a tag push (optional)
#   VERSION   explicit version from workflow_dispatch, "0.2.0" or "v0.2.0" (optional)
#   BUMP      patch (default), minor or major; used when neither is set
#   HEAD_SHA  commit to release (default HEAD)
#
# Rules:
#   TAG set      -> release that tag (it already exists; it was just pushed).
#   VERSION set  -> release vVERSION; skip (reason=exists) if that tag exists.
#   neither      -> bump BUMP from the latest v* tag (first release v0.1.1);
#                   skip (reason=tagged) if HEAD is already a release tag.
set -euf

here=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
head_sha=$(git rev-parse "${HEAD_SHA:-HEAD}")

valid() {
    printf '%s\n' "$1" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+$'
}
out() {
    echo "skip=$1"
    echo "reason=$2"
    echo "ver=$3"
    echo "tag=v$3"
    echo "bump=$4"
}

if [ -n "${TAG:-}" ]; then
    ver=${TAG#refs/tags/}
    ver=${ver#v}
    if ! valid "$ver"; then
        echo "invalid tag: ${TAG} (want vX.Y.Z)" >&2
        exit 1
    fi
    out no tag "$ver" ""
    exit 0
fi

if [ -n "${VERSION:-}" ]; then
    ver=${VERSION#v}
    if ! valid "$ver"; then
        echo "invalid version: ${VERSION} (want X.Y.Z)" >&2
        exit 1
    fi
    if git rev-parse -q --verify "refs/tags/v$ver" >/dev/null; then
        out yes exists "$ver" ""
        exit 0
    fi
    out no manual "$ver" ""
    exit 0
fi

if tag=$(git describe --tags --exact-match --match 'v[0-9]*' "$head_sha" 2>/dev/null); then
    out yes tagged "${tag#v}" ""
    exit 0
fi

bump=${BUMP:-patch}
case $bump in patch|minor|major) ;; *)
    echo "invalid bump: $bump (want patch, minor or major)" >&2
    exit 1 ;;
esac
latest=$(git tag -l 'v[0-9]*' --sort=-v:refname | head -n 1 || true)
if [ -z "$latest" ]; then
    out no manual 0.1.1 patch
    exit 0
fi
out no manual "$(sh "$here/next-version.sh" "$latest" "$bump")" "$bump"
