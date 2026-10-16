#!/bin/sh
# Skill copies must not drift: the operator skill and its plugin copy are
# identical, and every agent's dev skill path is a symlink to the canonical one.
set -eu
cd "$(dirname "$0")/.."
fail=0
if ! cmp -s skills/atr/SKILL.md plugins/atr-operator/skills/atr/SKILL.md; then
    echo "FAIL: plugins/atr-operator/skills/atr/SKILL.md differs from skills/atr/SKILL.md"
    fail=1
fi
for d in .claude .cursor .agents .grok; do
    p="$d/skills/atr-dev"
    if [ ! -L "$p" ] || [ "$(readlink "$p")" != "../../skills/atr-dev" ]; then
        echo "FAIL: $p must be a symlink to ../../skills/atr-dev"
        fail=1
    elif ! cmp -s "$p/SKILL.md" skills/atr-dev/SKILL.md; then
        echo "FAIL: $p/SKILL.md does not resolve to skills/atr-dev/SKILL.md"
        fail=1
    fi
done
[ "$fail" -eq 0 ] && echo "skills in sync"
exit "$fail"
