#!/bin/sh
# Next semver after a v* tag. Empty latest → first release 0.1.1.
# usage: next-version.sh [latest-tag] [patch|minor|major]
set -eu
latest=${1:-}
bump=${2:-patch}

if [ -z "$latest" ]; then
    echo "0.1.1"
    exit 0
fi

ver=${latest#v}
maj=${ver%%.*}
rest=${ver#*.}
min=${rest%%.*}
pat=${rest#*.}

case "$maj$min$pat" in
    *[!0-9]*)
        echo "invalid version: $latest" >&2
        exit 1
        ;;
esac

case "$bump" in
    major) echo "$((maj + 1)).0.0" ;;
    minor) echo "${maj}.$((min + 1)).0" ;;
    patch) echo "${maj}.${min}.$((pat + 1))" ;;
    *)
        echo "unknown bump: $bump" >&2
        exit 1
        ;;
esac
