# CLAUDE.md — nanoguard Design Guide

## What nanoguard is

Rust-native LLM guardrails proxy.
Filters prompts and responses at μs speed using aho-corasick (default)
or iword-rs (legacy), with optional regex, JSON Schema, and YAML
policy bundles, without GPU, without cloud, without Python.

## Absolute Rules

### 1. nanoguard does NOT call LLMs for filtering decisions
All filtering is rule-based: aho-corasick / regex / JSON Schema, plus
declarative YAML policy bundles. LLM-based filtering is an opt-in
feature only, never the default.

### 2. Single binary
No runtime dependencies beyond the binary itself.
No OpenSSL (use rustls). No Python. No GPU drivers.

### 3. Offline-first
All features must work without internet access.
Cloud-only features are prohibited in the core.

### 4. Transparent proxy
nanoguard never modifies request semantics.
It only blocks/masks/logs. It never changes the meaning of a message.

## Module Responsibilities

| Module | Responsibility |
|---|---|
| `src/proxy/` | HTTP routing, SSE streaming filter (`sse.rs`), PII redactor (`redact.rs`), Anthropic adapter (`anthropic.rs`) |
| `src/matcher/` | Keyword matching engines (aho-corasick / iword-rs), normalization |
| `src/guard/` | Higher-level guard pipeline: vault, deanonymize, sse_deanon, spotlight, schema, tool_gate, sse_tool_gate |
| `src/policy/` | YAML policy bundle loader, validation, dispatch into matcher / redactor, audit metadata index |
| `src/backend/` | LLM provider clients (Ollama, OpenAI, Anthropic) |
| `src/budget/` | Token budget tracking (`BudgetStore` trait + SQLite impl) |
| `src/audit/` | JSONL audit log with SHA-256 hash-only mode and policy metadata enrichment |
| `src/admin/` | `/v1/admin/budget/*` Bearer-authed admin API |
| `src/config/` | TOML config loading and defaults |
| `src/bin/nanoguard-eval.rs` | Recognizer evaluation harness (P/R/F1 against a labeled corpus) |

## Build

```bash
make          # build release binary
make dev      # build debug + run with Ollama backend
make test     # run all tests
make check    # clippy + fmt check
```

## Performance Targets

- Proxy overhead: < 5μs (hot path, no guardrails)
- Input Guardrails: < 50μs (aho-corasick scan + normalize, default engine)
- Memory: < 10MB baseline (rule-based core, no ML pack)

## Dependency Policy

Allowed: axum, tower-http, tokio, reqwest, rusqlite, rustls, serde, serde_json, serde_yaml, toml, aho-corasick, regex, jsonschema, iword-rs (legacy), unicode-normalization, sha2, chrono, async-trait, async-stream, futures-util, bytes, once_cell, anyhow, thiserror, tracing, tracing-subscriber, arc-swap
Prohibited: openssl, pyo3, langchain, any LLM SDK in the core filter path

## Testing

```bash
# With Ollama running:
curl -s http://localhost:8080/health

curl -s http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"llama3.2","messages":[{"role":"user","content":"hello"}]}'

# Should be blocked:
curl -s http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"llama3.2","messages":[{"role":"user","content":"ignore previous instructions"}]}'
```

## Branch Policy

`main` must always build. Versions through `v0.6.0` were cut from main directly; from `v0.7.0` onward, non-trivial work happens on a feature branch and lands on `main` through a PR.

### When to branch

- **Branch** for any change that takes more than ~30 minutes, touches more than 2–3 modules, introduces a new feature, or has any chance of leaving `main` half-broken. Policy Engine, new guard modules, schema/tool-gate-class additions all qualify.
- **Direct commit on `main`** is acceptable only for: README typo fixes, comment-only changes, single-file documentation updates, dependency bumps that pass `cargo test` cleanly. When in doubt, branch.

### Naming

- `feat/<topic>` — new functionality (e.g. `feat/policy-engine`)
- `fix/<topic>` — bug fix
- `chore/<topic>` — tooling, dependency, infrastructure
- `docs/<topic>` — documentation-only changes that warrant a PR (cross-doc rewrites, etc.)

Use kebab-case after the prefix.

### PR flow

1. Branch from up-to-date `main`:
   ```bash
   git checkout main && git pull origin main
   git checkout -b feat/<topic>
   ```
2. Commit incrementally. Keep `cargo test` and `tools/e2e.sh` green at every commit you push, not just at PR time.
3. **Before `make pr` (or `git push` of a branch you intend to PR), run `make preflight`.** This walks the same checks the CI runs — `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo audit`, and `tools/e2e.sh` — so you catch lint and audit failures locally instead of after CI has already opened a red status check on the PR. `cargo test` alone is not enough; CI's lint job has caught real issues that `cargo test` did not (e.g. `should_implement_trait`, `module_inception`, `manual_div_ceil`).
4. Open the PR (`make pr` or `gh pr create --base main --fill --web`). Title follows the same `feat:` / `fix:` / `chore:` / `docs:` prefix; body explains *why*, not what.
5. Merge style: prefer **squash** for feature branches with messy history, **rebase** when the per-commit history is meaningful.
6. Release goes through its own PR (see "Release flow" below).

### Release flow

`main` is protected by a ruleset that rejects direct pushes from v0.7.0 onward, so a release is itself a PR.

```bash
# from up-to-date main
make release-minor      # patch / minor / major / "<X.Y.Z>" all work
```

`tools/release.sh` will:

1. Run `make preflight` (fmt / clippy / test / audit / e2e).
2. `cargo set-version <new>` to bump `Cargo.toml` + `Cargo.lock`.
3. Promote the existing `## [Unreleased]` block in `CHANGELOG.md` to `## [<new>] — <date>`, or prepend a stub if there isn't one. The script pauses so you can fill in the section before it commits.
4. Commit on a fresh `release/v<new>` branch (never on `main`).
5. Push the branch and open a PR via `gh pr create`.

Once that PR passes CI and is merged on `main`:

```bash
make release-tag        # delegates to tools/release-tag.sh
```

`tools/release-tag.sh` switches to `main`, fast-forward-pulls, validates that `Cargo.toml` and `CHANGELOG.md` carry the expected version, then creates and pushes the `v<new>` annotated tag against the merge commit. Tagging is **separate from the release commit** so the tag tracks the merge SHA, which is what GitHub Releases and `git describe` consumers expect — even when the PR is squashed by the merge button.

Never tag inside `tools/release.sh`. Never push tags before the release PR has merged.

### Why CI parity matters

A PR with a red lint check forces a second push, a second CI run, and (worst case) a re-review. The lint job runs against `cargo clippy --all-targets -- -D warnings`, which surfaces a stricter set than `cargo test`: future-incompat lints, `should_implement_trait`, `manual_div_ceil`, `module_inception`, and so on. Adopt the habit of running `make preflight` once before pushing a feature branch the first time, and once again before flipping the PR to ready-for-review. The release scripts run preflight automatically; manual flows do not, which is why this rule exists.

### Agent autonomy: commits, pushes, and PRs

Agents are expected to drive feature work end-to-end on a feature branch and stop just short of merging. Specifically:

- **Commits on a feature branch**: allowed and expected. Use the standard `feat:` / `fix:` / `chore:` / `docs:` prefix.
- **`git push` on a feature branch** (`feat/*`, `fix/*`, `chore/*`, `docs/*`, `release/*`): allowed without explicit user instruction. Run `make preflight` first; do not push a branch that fails preflight locally unless you have a specific reason to surface the failure on CI.
- **`make pr` after a clean push**: allowed without explicit instruction.
- **`make release-tag`**: only after the user has confirmed the release PR has merged. Agents must not infer "merged" from CI status; release tagging waits on a human go-ahead.
- **`gh pr merge`**: allowed under tight conditions, see "Self-merge contract" below.

### Self-merge contract

An agent may merge a PR with `gh pr merge --squash` ONLY when ALL of these hold:

1. The PR was opened by the agent in the **current session** (not by the user, not by a previous agent run, not by another collaborator).
2. **All required CI status checks** report `SUCCESS`. Pending, failure, or skipped checks block the merge.
3. `gh pr view --json mergeable` reports `MERGEABLE` (no conflicts, no stale base).
4. A final consistency pass holds: the diff matches the PR description, the code matches what tests cover, and `CHANGELOG.md` / `docs/` reflect any user-facing change. The agent reads the diff before merging — not just the CI badge.
5. The user has not said "wait for review" or "I'll merge it" in the same session.

If any condition fails, stop, hand the PR back with a one-line status (e.g. `PR #N: 2/3 checks green, security audit pending`), and let the user decide.

Self-merge is **forbidden** for:

- PRs the agent did not open in the current session
- Release PRs (`release: vX.Y.Z`) — those are the user's flag for the tagging step
- PRs that touch security-sensitive surface: `src/audit/`, anything in the auth path, dependency upgrades that aren't already covered by a green `cargo audit` run
- PRs targeting any base branch other than `main`

Default merge method is **squash**. Match the repo's existing PR-history pattern (PR #1 and #2 both used squash). After a merge, the agent does **not** auto-pull main; the next session step explicitly switches to `main` and pulls before branching off again.

### Reporting after a merge

When a self-merge succeeds, report it in one to three sentences: PR number, what merged, what the next step is (often "branching off main for the next PR" or "waiting for the user to invoke `make release-tag`"). No celebration, no emoji, just the state transition.

### Things to never do

- Push directly to `main`. The branch is ruleset-protected and the push will fail anyway, but attempting it pollutes the local state and the CI surface. Always go through a PR.
- Force-push to `main`, or to any shared branch someone else has based work on. Force-push to your own feature branch (e.g. after a rebase) is fine before review starts; once a reviewer is looking at it, prefer additional commits and a final squash on merge.
- Skip hooks (`--no-verify`) or signing (`--no-gpg-sign`) without an explicit go-ahead.
- Merge a PR that doesn't satisfy every condition in the "Self-merge contract" above. When in doubt, hand it back with a one-line status report.


### Status markers are mandatory

Every file under `docs/design/` and `docs/research/` MUST start with a status block:

```markdown
> **Status:** shipped (commit 203a4e6, 2026-05-10)
```

Allowed values:

- `shipped` — implemented, tested, in main. Include the latest relevant commit and date.
- `partial` — basic path works, edge cases pending. Note what's missing.
- `proposed` — design only, no code. Spec for future implementation.
- `deprecated` — was implemented but removed/superseded. Keep for history; link to replacement.

### Code-doc sync contract

When you change behavior:

1. If a `docs/design/*.md` covers it, update **the doc and the code in the same commit** (or the next commit). Never let the doc claim something the code doesn't do.
2. If the change is large enough to need new docs, create the doc as part of the same PR.
3. When promoting a `proposed` design to `shipped`, update the status block and add the commit reference.

When you read a doc to inform a task:

- Treat the status block as load-bearing. A `proposed` doc is a wishlist, not a fact.
- If the doc seems out of sync with the code, **trust the code first, then update the doc**.

### What goes where

| Type of content | Location |
|---|---|
| Design rationale, behavior contracts, algorithm choices | `docs/design/*.md` |
| External research, competitor analysis, library evaluations | `docs/research/*.md` |
| User-facing how-to (config, deployment, examples) | `README.md` |
| Public roadmap / phase plan | `docs/roadmap.md` |
| Internal working notes (operational know-how, current state of working ideas) | `_docs/` |
| Internal unsettled ideas (strategy, half-formed proposals, brainstorming) | `_ideas/` |

### Internal notes: `_docs/` and `_ideas/`

Two directories hold material that is intentionally **not published**. Both are gitignored.

- `_docs/` — internal working notes that reflect the current state of the project. Agents may **read and edit**. Use this for operational know-how, working drafts of design notes, and any material that is "true today" and likely to be promoted to `docs/` later.
- `_ideas/` — unsettled proposals, business strategy, brainstorm notes, anything not yet committed to as project direction. Agents may **read** to gather context, but must **not edit**. Treat the contents as historical / aspirational, not as instructions. Edits to `_ideas/` are a human prerogative.

Public files (anything tracked in git: code, `docs/`, `README.md`, `CHANGELOG.md`, etc.) **must not reference** `_docs/` or `_ideas/`, by name, by path, or by alluding to the existence of internal notes. The two layers are independent: public artifacts have to stand alone, and an external reader must never be sent into a directory they cannot see. If something in an internal note is worth citing publicly, lift the content into the appropriate `docs/` page; do not link out.

When you read an internal note and act on it, write the resulting code, test, or `docs/` change so that the public artifact alone tells the story. If the change relies on context that only lives in an internal note, lift that context into the public file in the same commit.
