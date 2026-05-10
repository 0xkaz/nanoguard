#!/usr/bin/env bash
# Create and push the vX.Y.Z tag for a release that has already landed
# on main via PR. Pair with tools/release.sh, which prepares the release
# branch and PR.
#
# Usage:
#   ./tools/release-tag.sh             # infer version from main's Cargo.toml
#   ./tools/release-tag.sh 0.8.0       # explicit version (must match Cargo.toml)
#
# Flow:
#   1. switch to main, fast-forward pull from origin
#   2. read Cargo.toml's version (or use the explicit arg)
#   3. confirm CHANGELOG has the matching `## [<version>]` section
#   4. refuse if the tag already exists locally or on origin
#   5. annotate-tag the current main HEAD and push the tag
#
# This is the *only* step in the release flow that touches a tag — release.sh
# never tags. That keeps the tag pointing at the merge commit, not at a
# branch commit that might be squashed away by GitHub's merge button.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

EXPECTED="${1:-}"

# --- 1. switch to main and pull -------------------------------------------

if [ -n "$(git status --porcelain)" ]; then
    echo "error: working tree has uncommitted changes:" >&2
    git status --short >&2
    exit 1
fi

START_BRANCH="$(git rev-parse --abbrev-ref HEAD)"
if [ "$START_BRANCH" != "main" ]; then
    echo "→ switching to main (was on $START_BRANCH)"
    git checkout main
fi

echo "→ git pull origin main --ff-only"
git pull origin main --ff-only

# --- 2. determine version --------------------------------------------------

CURRENT="$(awk -F\" '/^version *= *"/ {print $2; exit}' Cargo.toml)"
if [ -z "$CURRENT" ]; then
    echo "error: could not parse version from Cargo.toml" >&2
    exit 1
fi

if [ -n "$EXPECTED" ] && [ "$EXPECTED" != "$CURRENT" ]; then
    echo "error: requested version $EXPECTED but main's Cargo.toml is $CURRENT" >&2
    echo "  did the release PR get merged?" >&2
    exit 1
fi

NEW="$CURRENT"
TAG="v$NEW"

# --- 3. confirm CHANGELOG --------------------------------------------------

if [ -f CHANGELOG.md ] && ! grep -q "^## \[$NEW\]" CHANGELOG.md; then
    echo "warning: CHANGELOG.md has no [$NEW] section."
    echo "  the release commit may not have updated CHANGELOG."
    read -r -p "  tag $TAG anyway? [y/N] " ans
    case "$ans" in y|Y|yes|YES) ;; *) echo "aborted"; exit 0 ;; esac
fi

# --- 4. tag-existence check ------------------------------------------------

git fetch --tags origin > /dev/null 2>&1 || true
if git rev-parse --verify --quiet "$TAG" > /dev/null; then
    echo "error: tag $TAG already exists locally" >&2
    echo "  delete with: git tag -d $TAG && git push origin :refs/tags/$TAG" >&2
    exit 1
fi
if git ls-remote --exit-code --tags origin "$TAG" > /dev/null 2>&1; then
    echo "error: tag $TAG already exists on origin" >&2
    exit 1
fi

# --- 5. tag and push -------------------------------------------------------

HEAD_SHORT="$(git rev-parse --short HEAD)"
echo ""
echo "tag:    $TAG"
echo "target: main HEAD ($HEAD_SHORT)"
echo "        $(git log --oneline -1)"
echo ""
read -r -p "create and push $TAG? [Y/n] " ans
case "$ans" in n|N|no|NO) echo "aborted"; exit 0 ;; esac

git tag -a "$TAG" -m "v$NEW

See CHANGELOG.md for release notes."
git push origin "$TAG"

echo ""
echo "✓ $TAG tagged and pushed"
echo ""
echo "next:"
echo "  - draft a GitHub Release (gh release create $TAG --notes-file CHANGELOG.md or via UI)"
echo "  - if a release workflow auto-publishes a Docker image, watch for it on ghcr.io"
