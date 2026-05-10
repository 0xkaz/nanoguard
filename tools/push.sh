#!/usr/bin/env bash
# Push main and any unreleased tags to origin.
#
# Use this when a release commit + tag have already been created locally
# (for example by tools/release.sh) and you just want to publish them.
# For a full bump-test-tag-push flow, use tools/release.sh instead.
#
# Usage: ./tools/push.sh

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if ! git rev-parse --is-inside-work-tree > /dev/null 2>&1; then
    echo "error: not inside a git worktree" >&2
    exit 1
fi

if ! git remote get-url origin > /dev/null 2>&1; then
    echo "error: no 'origin' remote configured" >&2
    exit 1
fi

if [ -n "$(git status --porcelain)" ]; then
    echo "error: working tree has uncommitted changes:" >&2
    git status --short >&2
    exit 1
fi

BRANCH="$(git rev-parse --abbrev-ref HEAD)"
if [ "$BRANCH" != "main" ]; then
    echo "warning: current branch is '$BRANCH', not 'main'"
    read -r -p "push '$BRANCH' anyway? [y/N] " ans
    case "$ans" in y|Y|yes|YES) ;; *) echo "aborted"; exit 0 ;; esac
fi

# Tags reachable from HEAD that haven't been pushed to origin yet.
git fetch --tags origin > /dev/null 2>&1 || true
LOCAL_TAGS=$(git tag --merged HEAD --list 'v*')
REMOTE_TAGS=$(git ls-remote --tags origin | awk '{print $2}' | sed -e 's|refs/tags/||' -e 's|\^{}||' | sort -u)
UNPUSHED_TAGS=()
for t in $LOCAL_TAGS; do
    if ! grep -q -x "$t" <<<"$REMOTE_TAGS"; then
        UNPUSHED_TAGS+=("$t")
    fi
done

echo "branch: $BRANCH"
echo "ahead of origin/$BRANCH:"
git log --oneline "origin/$BRANCH..HEAD" 2>/dev/null || echo "  (no upstream tracking — first push)"
if [ "${#UNPUSHED_TAGS[@]}" -gt 0 ]; then
    echo "tags to push: ${UNPUSHED_TAGS[*]}"
else
    echo "tags to push: (none)"
fi
echo ""
read -r -p "proceed? [y/N] " ans
case "$ans" in y|Y|yes|YES) ;; *) echo "aborted"; exit 0 ;; esac

git push origin "$BRANCH"
for t in "${UNPUSHED_TAGS[@]}"; do
    git push origin "$t"
done

echo ""
echo "✓ pushed"
