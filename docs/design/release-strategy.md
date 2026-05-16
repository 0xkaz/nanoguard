> **Status:** proposed (2026-05-17)

# Release Strategy

This document reviews the current release process for nanoguard and proposes
a strategy to scale distribution, automate labor-intensive steps, and
strengthen the security of the release pipeline.

## Current State

As of v0.7.0, the release flow is a two-step PR-driven process managed by local scripts:

1. **`make release-minor`** (Step 1):
   - Runs `make preflight` (tests, lint, audit, e2e) locally.
   - Bumps `Cargo.toml` version.
   - Updates `CHANGELOG.md` (promotes `[Unreleased]` to a dated version).
   - Commits and pushes a `release/vX.Y.Z` branch.
   - Opens a GitHub PR.
2. **Merge PR** (Manual):
   - User or Agent merges the PR on GitHub.
3. **`make release-tag`** (Step 2):
   - Pulls `main` locally.
   - Creates an annotated git tag.
   - Pushes the tag to `origin`.
4. **Post-Tag Automation**:
   - GitHub Action `release.yml` triggers on tag push.
   - Builds multi-arch Docker images (AMD64/ARM64).
   - Pushes images to GHCR.

### Bottlenecks and Risks

- **Manual Tagging**: Step 2 requires a local environment with `git` push access and a clean `main` branch. This is the only step that cannot be fully automated in the cloud without a PAT or App token.
- **Binary Distribution**: Users must build from source or use Docker. There are no pre-compiled binaries for Linux/macOS/Windows on GitHub Releases.
- **Homebrew/Crates.io**: No official distribution through these package managers.
- **Changelog Management**: While `tools/release.sh` assists, the summary of changes is still a manual `<!-- TODO -->` stub.

## Proposed Strategy

### Phase 1: Distribution Hardening (Short-term)

1. **Automated GitHub Releases**:
   - Update `release.yml` to use `softprops/action-gh-release`. (✅ Shipped)
   - Automatically create a GitHub Release when a tag is pushed. (✅ Shipped)
   - Use `CHANGELOG.md` content as the release notes.
2. **Binary Artifacts**:
   - Integrate `cargo-dist` or a custom Action job to build and upload binaries for `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, and `x86_64-apple-darwin` to the GitHub Release. (✅ Shipped via custom Action job)
3. **Homebrew Tap**:
   - Create `0xkaz/homebrew-tap` and automate formula updates via a GitHub Action triggered by new releases.

### Phase 2: Workflow Automation (Mid-term)

1. **Release-on-Merge**:
   - Shift the logic of `tools/release-tag.sh` to a GitHub Action.
   - When a PR with a `release` label is merged, the Action automatically creates the tag.
   - This eliminates the need for Step 2 to be run locally.
2. **Conventional Commits**:
   - Adopt Conventional Commits (`feat:`, `fix:`, `chore:`) to automate `CHANGELOG.md` generation.
   - Use `git-cliff` or `standard-version` to keep the log in sync with the commit history.

### Phase 3: Platform Expansion (Long-term)

1. **Crates.io Publishing**:
   - Publish `nanoguard` as a library/binary to Crates.io once the API is stable.
2. **Edge Packaging**:
   - As explored in `docs/research/edge-deployment.md`, provide specialized packaging for edge environments (e.g., optimized containers for Fly.io, AWS Lambda adapters).

## Success Criteria

- **Zero-touch Tagging**: A release can be fully executed by merging a PR.
- **Multi-channel Presence**: Users can `brew install`, `docker pull`, or download a binary directly.
- **Verified Supply Chain**: Binaries and images are signed (e.g., via Sigstore/Cosign).

## Implementation Plan

1. Create `docs/design/release-strategy.md` (this document).
2. Update `release.yml` to include GitHub Release creation.
3. Prototype `cargo-dist` integration.
