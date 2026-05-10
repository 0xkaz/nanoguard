# tools/

Local development and release helpers. Not shipped in the binary.

## Files

| File | Purpose |
|---|---|
| `mock_backend.py` | Minimal OpenAI-compatible echo backend used by `e2e.sh`. Has a `TOOL:` hook for deterministic streaming/non-streaming tool calls. |
| `e2e.toml` | nanoguard config used by `e2e.sh` (binds to `:18080`, points at the mock backend on `:11500`, enables reversible PII redaction). |
| `e2e.sh` | End-to-end smoke test. Boots the mock backend and the release binary, runs 19 scenarios (41 assertions). Covers health, prompt-injection block, PII redaction (mask + reversible round-trip), AWS-key reject, Anthropic redaction, Anthropic `tool_use` round-trip, denied-tool surfacing as `nanoguard_denied_tools` block, indexed placeholders, streaming SSE filter + deanonymize, shadow mode, spotlighting, output schema (log + reject), tool gate (allow/deny on chat_completions), recognizer eval, policy bundle audit enrichment, streaming tool gate deny, streaming budget usage, and the Anthropic stream=true refusal path. Used by `make preflight` and CI. |
| `release.sh` | Step 1 of release: preflight → bump version → branch → commit → push branch → open PR. Never tags. |
| `release-tag.sh` | Step 2 of release: after the release PR has merged, switch to main, validate, tag, push. |
| `push.sh` | Push the current branch and any unreleased tags reachable from `HEAD` to `origin`. Refuses with a dirty working tree. |

## End-to-end test

```bash
./tools/e2e.sh
```

Requires `python3`, `curl`, `jq`. Builds the release binary if missing. Logs land in `/tmp/nanoguard-e2e/` (override with `LOGDIR=...`).

## Preflight (run before any PR)

```bash
make preflight
```

Walks the same jobs CI runs: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo audit`, `tools/e2e.sh`. Required by CLAUDE.md before opening a PR.

`cargo audit` is required (not skip-with-warning); install once with `cargo install cargo-audit`.

## Cutting a release

Install once per machine:

```bash
cargo install cargo-edit cargo-audit
```

`main` is protected by a ruleset, so a release is itself a PR. Two-step flow:

### Step 1: open the release PR

```bash
make release-minor       # patch / minor / major / explicit "<X.Y.Z>" all work
# under the hood: ./tools/release.sh minor
```

`release.sh` will:

1. Run `make preflight`.
2. `cargo set-version <new>` to bump `Cargo.toml` + `Cargo.lock`.
3. Promote the existing `## [Unreleased]` section in `CHANGELOG.md` to `## [<new>] — <date>` (or prepend a stub if there isn't one). Pauses for you to fill in the section.
4. Commit on a fresh `release/v<new>` branch (never on `main`).
5. Push the branch and open a PR via `gh`.

Flags:

- `--skip-preflight` — skip the make preflight step (NOT recommended)
- `--skip-pr` — stop after creating the local release branch (push and PR by hand)
- `--dry-run` — print the plan and exit

### Step 2: tag after merge

After the release PR is green and merged on `main`:

```bash
make release-tag
# under the hood: ./tools/release-tag.sh
```

`release-tag.sh` switches to `main`, fast-forward-pulls, validates that `Cargo.toml` and `CHANGELOG.md` agree on the version, and creates/pushes the annotated tag against the merge commit. The tag goes in only at this step so it tracks the merge SHA — even when GitHub squashes the PR.

Tagging is intentionally **not** part of `release.sh`. Never tag a release branch.

## Pushing without a release

For ad-hoc feature-branch pushes:

```bash
./tools/push.sh
# or: make push
```

`push.sh` pushes the current branch and any locally-existing tags reachable from `HEAD` that aren't yet on `origin`. Refuses with a dirty working tree. Note that `main` is protected by a ruleset; pushing directly to `main` will fail.

## Why these scripts exist

`make release` (in the existing Makefile) builds and pushes a Docker image. It does **not** bump the version, run tests, or create a git tag. The scripts here cover the source-side release flow so the Docker image and the git tag stay in sync, and so the entire flow respects the main-branch ruleset.
