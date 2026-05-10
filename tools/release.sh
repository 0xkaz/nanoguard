#!/usr/bin/env bash
# Cut a new release through a PR.
#
# Usage:
#   ./tools/release.sh patch        # 0.7.0 → 0.7.1
#   ./tools/release.sh minor        # 0.7.0 → 0.8.0
#   ./tools/release.sh major        # 0.7.0 → 1.0.0
#   ./tools/release.sh 0.8.0-rc.1   # explicit version
#
# Flags:
#   --skip-preflight     skip cargo fmt/clippy/test/audit/e2e (NOT recommended)
#   --skip-pr            stop after creating the release branch (don't run gh)
#   --dry-run            print the plan and exit without changing anything
#
# Flow:
#   1. preflight (fmt + clippy + test + audit + e2e)
#   2. cargo set-version (bumps Cargo.toml + Cargo.lock)
#   3. CHANGELOG.md: rename [Unreleased] → [<new>] or prepend a stub
#   4. commit on a fresh release/v<new> branch (NOT main)
#   5. push the branch and open a PR via `gh`
#
# After the PR is merged on main, run `make release-tag` to create and
# push the v<new> tag pointing at the merge commit.
#
# Background: from v0.7.0 onward, main is protected by a ruleset that
# rejects direct pushes. The previous flow that committed straight to
# main and then pushed the tag tripped that rule on every release. This
# script switches to PR-driven releases so the same protection applies
# to the version bump itself.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# --- arg parsing -----------------------------------------------------------

BUMP=""
DO_PR=1
SKIP_PREFLIGHT=0
DRY_RUN=0

while [ $# -gt 0 ]; do
    case "$1" in
        patch|minor|major) BUMP="$1"; shift ;;
        --skip-preflight) SKIP_PREFLIGHT=1; shift ;;
        --skip-pr)        DO_PR=0; shift ;;
        --dry-run)        DRY_RUN=1; shift ;;
        -h|--help)
            sed -n '2,28p' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        [0-9]*) BUMP="explicit:$1"; shift ;;
        *) echo "error: unknown arg \`$1\`" >&2; exit 1 ;;
    esac
done

if [ -z "$BUMP" ]; then
    echo "error: specify patch / minor / major / <version>" >&2
    echo "       (run with --help for usage)" >&2
    exit 1
fi

# --- preflight -------------------------------------------------------------

if ! cargo set-version --help > /dev/null 2>&1; then
    echo "error: \`cargo set-version\` not available." >&2
    echo "       install it with: cargo install cargo-edit" >&2
    exit 1
fi

if ! command -v gh > /dev/null 2>&1 && [ "$DO_PR" -eq 1 ]; then
    echo "error: \`gh\` CLI not found but --skip-pr was not given." >&2
    echo "       install: https://cli.github.com  or pass --skip-pr" >&2
    exit 1
fi

if [ -n "$(git status --porcelain)" ]; then
    echo "error: working tree has uncommitted changes:" >&2
    git status --short >&2
    exit 1
fi

START_BRANCH="$(git rev-parse --abbrev-ref HEAD)"
if [ "$START_BRANCH" != "main" ]; then
    echo "warning: starting from '$START_BRANCH', not 'main'."
    echo "  release branches are usually cut from main."
    read -r -p "  proceed from '$START_BRANCH' anyway? [y/N] " ans
    case "$ans" in y|Y|yes|YES) ;; *) echo "aborted"; exit 0 ;; esac
fi

# --- compute new version ---------------------------------------------------

CURRENT="$(awk -F\" '/^version *= *"/ {print $2; exit}' Cargo.toml)"
if [ -z "$CURRENT" ]; then
    echo "error: could not parse version from Cargo.toml" >&2
    exit 1
fi

if [[ "$BUMP" == explicit:* ]]; then
    NEW="${BUMP#explicit:}"
else
    IFS='.' read -r MAJ MIN PAT <<<"$CURRENT"
    PAT="${PAT%%-*}"
    case "$BUMP" in
        patch) NEW="$MAJ.$MIN.$((PAT + 1))" ;;
        minor) NEW="$MAJ.$((MIN + 1)).0" ;;
        major) NEW="$((MAJ + 1)).0.0" ;;
    esac
fi

TAG="v$NEW"
RELEASE_BRANCH="release/$TAG"

git fetch --tags origin > /dev/null 2>&1 || true
if git rev-parse --verify --quiet "$TAG" > /dev/null; then
    echo "error: tag $TAG already exists locally" >&2
    exit 1
fi
if git rev-parse --verify --quiet "refs/remotes/origin/$RELEASE_BRANCH" > /dev/null; then
    echo "error: branch $RELEASE_BRANCH already exists on origin" >&2
    echo "  delete it first with: git push origin :$RELEASE_BRANCH" >&2
    exit 1
fi

echo "bump:    $CURRENT  →  $NEW"
echo "tag:     $TAG (created later by \`make release-tag\` after PR merge)"
echo "branch:  $START_BRANCH  →  $RELEASE_BRANCH"
echo "preflight: $([ "$SKIP_PREFLIGHT" -eq 1 ] && echo skip || echo "fmt + clippy + test + audit + e2e")"
echo "PR:      $([ "$DO_PR" -eq 1 ] && echo "gh pr create --base main" || echo "skip")"
echo ""

if [ "$DRY_RUN" -eq 1 ]; then
    echo "dry-run: nothing changed."
    exit 0
fi

read -r -p "proceed? [y/N] " ans
case "$ans" in y|Y|yes|YES) ;; *) echo "aborted"; exit 0 ;; esac

# --- 1. preflight ----------------------------------------------------------

if [ "$SKIP_PREFLIGHT" -eq 0 ]; then
    echo "→ preflight"
    make preflight
fi

# --- 2. branch + bump + CHANGELOG -----------------------------------------

echo "→ creating $RELEASE_BRANCH"
git checkout -b "$RELEASE_BRANCH"

echo "→ cargo set-version $NEW"
cargo set-version "$NEW"

DATE="$(date +%Y-%m-%d)"
if [ -f CHANGELOG.md ]; then
    if grep -q "^## \[$NEW\]" CHANGELOG.md; then
        echo "→ CHANGELOG already has [$NEW] section, leaving as-is"
    elif grep -q "^## \[Unreleased\]" CHANGELOG.md; then
        # Promote the [Unreleased] section to [<new>] — date it today.
        echo "→ promoting [Unreleased] → [$NEW] in CHANGELOG"
        sed -i.bak "s/^## \[Unreleased\].*/## [$NEW] — $DATE/" CHANGELOG.md
        rm CHANGELOG.md.bak
    else
        echo "→ prepending [$NEW] stub to CHANGELOG"
        TMP="$(mktemp)"
        {
            awk '
                /^## \[/ { exit }
                { print }
            ' CHANGELOG.md
            cat <<EOF
## [$NEW] — $DATE

<!-- TODO: summarize this release. Delete this stub if intentionally empty. -->

EOF
            awk '
                BEGIN { skipping = 1 }
                /^## \[/ { skipping = 0 }
                !skipping
            ' CHANGELOG.md
        } > "$TMP"
        mv "$TMP" CHANGELOG.md
        echo ""
        echo "  Edit CHANGELOG.md in another shell to fill in v$NEW notes."
        read -r -p "  press ENTER when CHANGELOG.md is ready (or Ctrl+C to abort)..."
    fi
fi

# --- 3. commit -------------------------------------------------------------

git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "release: v$NEW

See CHANGELOG.md for details."

echo ""
echo "✓ release branch ready"
echo "  $(git log --oneline -1)"

# --- 4. push + PR ----------------------------------------------------------

if [ "$DO_PR" -eq 1 ]; then
    echo ""
    read -r -p "push $RELEASE_BRANCH and open a PR? [Y/n] " ans
    case "$ans" in n|N|no|NO)
        echo ""
        echo "skipped push. run later:"
        echo "  git push -u origin $RELEASE_BRANCH"
        echo "  gh pr create --base main --head $RELEASE_BRANCH --title 'release: v$NEW' --fill"
        exit 0
        ;;
    esac
    git push -u origin "$RELEASE_BRANCH"
    gh pr create \
        --base main \
        --head "$RELEASE_BRANCH" \
        --title "release: v$NEW" \
        --body "Cuts v$NEW from main. See CHANGELOG.md for the v$NEW section.

After this PR is merged, run \`make release-tag\` from main to create and push the \`$TAG\` tag.

Generated by \`tools/release.sh\`."
    echo ""
    echo "✓ PR opened. Next steps:"
    echo "  1. wait for CI to go green"
    echo "  2. merge the PR"
    echo "  3. \`make release-tag\` to create and push $TAG"
else
    echo ""
    echo "skipped PR (--skip-pr). run later:"
    echo "  git push -u origin $RELEASE_BRANCH"
    echo "  gh pr create --base main --head $RELEASE_BRANCH"
fi
