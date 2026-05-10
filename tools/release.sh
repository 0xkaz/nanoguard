#!/usr/bin/env bash
# Cut a new release: bump version, run tests + e2e, commit, tag, push.
#
# Usage:
#   ./tools/release.sh patch        # 0.3.0 → 0.3.1
#   ./tools/release.sh minor        # 0.3.0 → 0.4.0
#   ./tools/release.sh major        # 0.3.0 → 1.0.0
#   ./tools/release.sh 0.4.0-rc.1   # explicit version
#
# Flags:
#   --no-push            stop after tagging (do `tools/push.sh` later)
#   --skip-e2e           skip tools/e2e.sh (cargo test still runs)
#   --skip-tests         skip cargo test AND e2e (useful for docs-only bumps)
#   --dry-run            print the plan and exit without changing anything
#
# Preconditions: clean working tree, on `main`, all changes for this release
# already committed. The script uses `cargo set-version` (cargo-edit) to bump
# Cargo.toml + Cargo.lock, runs the test suite, prepends a CHANGELOG entry
# stub, and lets you edit the CHANGELOG before committing.
#
# Install cargo-edit if needed:
#   cargo install cargo-edit

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# --- arg parsing -----------------------------------------------------------

BUMP=""
DO_PUSH=1
RUN_E2E=1
RUN_TESTS=1
DRY_RUN=0

while [ $# -gt 0 ]; do
    case "$1" in
        patch|minor|major) BUMP="$1"; shift ;;
        --no-push)   DO_PUSH=0; shift ;;
        --skip-e2e)  RUN_E2E=0; shift ;;
        --skip-tests) RUN_TESTS=0; RUN_E2E=0; shift ;;
        --dry-run)   DRY_RUN=1; shift ;;
        -h|--help)
            sed -n '2,17p' "$0" | sed 's/^# \?//'
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

if [ -n "$(git status --porcelain)" ]; then
    echo "error: working tree has uncommitted changes:" >&2
    git status --short >&2
    exit 1
fi

BRANCH="$(git rev-parse --abbrev-ref HEAD)"
if [ "$BRANCH" != "main" ]; then
    echo "warning: current branch is '$BRANCH', not 'main'"
    read -r -p "release from '$BRANCH' anyway? [y/N] " ans
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
    # Strip pre-release suffix from PAT if any (e.g. "1-rc.1" → "1").
    PAT="${PAT%%-*}"
    case "$BUMP" in
        patch) NEW="$MAJ.$MIN.$((PAT + 1))" ;;
        minor) NEW="$MAJ.$((MIN + 1)).0" ;;
        major) NEW="$((MAJ + 1)).0.0" ;;
    esac
fi

TAG="v$NEW"

# Refuse if tag already exists (locally or on origin).
git fetch --tags origin > /dev/null 2>&1 || true
if git rev-parse --verify --quiet "$TAG" > /dev/null; then
    echo "error: tag $TAG already exists locally" >&2
    exit 1
fi

echo "bump:    $CURRENT  →  $NEW   (tag: $TAG)"
echo "branch:  $BRANCH"
echo "tests:   $([ "$RUN_TESTS" -eq 1 ] && echo cargo test || echo skip)"
echo "e2e:     $([ "$RUN_E2E" -eq 1 ] && echo tools/e2e.sh || echo skip)"
echo "push:    $([ "$DO_PUSH" -eq 1 ] && echo "main + $TAG" || echo "skip (use tools/push.sh)")"
echo ""

if [ "$DRY_RUN" -eq 1 ]; then
    echo "dry-run: nothing changed."
    exit 0
fi

read -r -p "proceed? [y/N] " ans
case "$ans" in y|Y|yes|YES) ;; *) echo "aborted"; exit 0 ;; esac

# --- 1. bump Cargo.toml + Cargo.lock --------------------------------------

# `cargo set-version` updates Cargo.toml and refreshes Cargo.lock atomically.
cargo set-version "$NEW"

# --- 2. CHANGELOG stub -----------------------------------------------------

DATE="$(date +%Y-%m-%d)"
if [ -f CHANGELOG.md ]; then
    if ! grep -q "^## \[$NEW\]" CHANGELOG.md; then
        TMP="$(mktemp)"
        {
            # Keep top-of-file preamble (everything before the first version section).
            awk '
                /^## \[/ { exit }
                { print }
            ' CHANGELOG.md
            cat <<EOF
## [$NEW] — $DATE

<!-- TODO: summarize this release. Delete this stub if intentionally empty. -->

EOF
            # Then the rest of the file (existing version sections).
            awk '
                BEGIN { skipping = 1 }
                /^## \[/ { skipping = 0 }
                !skipping
            ' CHANGELOG.md
        } > "$TMP"
        mv "$TMP" CHANGELOG.md
        echo ""
        echo "→ A stub for v$NEW has been prepended to CHANGELOG.md."
        echo "  Edit it in another shell, then return here to continue."
        read -r -p "  press ENTER when CHANGELOG.md is ready (or Ctrl+C to abort)..."
    fi
fi

# --- 3. tests --------------------------------------------------------------

if [ "$RUN_TESTS" -eq 1 ]; then
    echo "→ cargo test"
    cargo test --quiet
fi

if [ "$RUN_E2E" -eq 1 ]; then
    echo "→ cargo build --release (for e2e)"
    cargo build --release --quiet
    echo "→ tools/e2e.sh"
    ./tools/e2e.sh
fi

# --- 4. commit + tag -------------------------------------------------------

git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "release: v$NEW

See CHANGELOG.md for details."

git tag -a "$TAG" -m "v$NEW

See CHANGELOG.md for release notes."

echo ""
echo "✓ committed and tagged $TAG"
echo "  $(git log --oneline -1)"

# --- 5. push ---------------------------------------------------------------

if [ "$DO_PUSH" -eq 1 ]; then
    echo ""
    read -r -p "push main and $TAG to origin? [y/N] " ans
    case "$ans" in
        y|Y|yes|YES)
            git push origin "$BRANCH"
            git push origin "$TAG"
            echo "✓ pushed"
            ;;
        *)
            echo "skipped push. Run \`./tools/push.sh\` later."
            ;;
    esac
else
    echo ""
    echo "skipped push (--no-push). Run \`./tools/push.sh\` when ready."
fi
