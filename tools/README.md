# tools/

Local development and release helpers. Not shipped in the binary.

## Files

| File | Purpose |
|---|---|
| `mock_backend.py` | Minimal OpenAI-compatible echo backend used by `e2e.sh`. |
| `e2e.toml` | nanoguard config used by `e2e.sh` (binds to `:18080`, points at the mock backend on `:11500`, enables reversible PII redaction). |
| `e2e.sh` | End-to-end smoke test. Boots the mock backend and the release binary, runs 19 scenarios (37 assertions). Covers health, prompt-injection block, PII redaction (mask + reversible round-trip), AWS-key reject, Anthropic redaction, indexed placeholders, streaming SSE filter + deanonymize, shadow mode, spotlighting, output schema (log + reject), tool gate (allow/deny + Anthropic), recognizer eval, policy bundle audit enrichment, streaming tool gate deny, streaming budget usage, and the Anthropic stream=true refusal path. Used by `release.sh` and CI. |
| `release.sh` | Bump version → run tests + e2e → commit → tag → push. |
| `push.sh` | Push `main` and any unreleased tags reachable from `HEAD` to `origin`. Used standalone or as the trailing step of `release.sh`. |

## End-to-end test

Run:

```bash
./tools/e2e.sh
```

Requires `python3`, `curl`, `jq`. Builds the release binary if missing.

Logs land in `/tmp/nanoguard-e2e/` (override with `LOGDIR=...`).

## Cutting a release

Install `cargo-edit` once:

```bash
cargo install cargo-edit
```

Then bump and push in one shot:

```bash
./tools/release.sh patch     # 0.3.0 → 0.3.1
./tools/release.sh minor     # 0.3.0 → 0.4.0
./tools/release.sh major     # 0.3.0 → 1.0.0
./tools/release.sh 0.4.0-rc.1   # explicit version
```

Or via Make:

```bash
make release-patch
make release-minor
make release-major
```

Flags:

- `--no-push` — stop after the local commit/tag (run `tools/push.sh` later)
- `--skip-e2e` — skip `tools/e2e.sh` (cargo test still runs)
- `--skip-tests` — skip cargo test AND e2e (docs-only bumps)
- `--dry-run` — print the plan without changing anything

The script:

1. Verifies clean working tree, current branch is `main` (asks if not).
2. Computes the new version with `cargo set-version`.
3. Refuses if the target tag already exists locally.
4. Prepends a `## [<version>] — <date>` stub to `CHANGELOG.md` (if missing) and pauses for you to edit.
5. Runs `cargo test`, then `cargo build --release`, then `tools/e2e.sh`.
6. Commits `Cargo.toml` + `Cargo.lock` + `CHANGELOG.md`, tags, asks before pushing.

## Pushing without a release

When `release.sh` was run with `--no-push`, or you have ad-hoc commits on `main` that should reach origin:

```bash
./tools/push.sh
# or: make push
```

`push.sh` only pushes the current branch and any locally-existing tags reachable from `HEAD` that are not yet on origin. It refuses to run with a dirty working tree.

## Why these scripts exist

`make release` (in the existing Makefile) builds and pushes a Docker image. It does **not** bump the version, run tests, or create a git tag. The scripts here cover the source-side release flow so that the Docker image and the git tag stay in sync.
