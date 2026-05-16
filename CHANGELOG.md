# Changelog

All notable changes to nanoguard are documented in this file. The format is loosely based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Docs — hot-reload proposal

New file `docs/design/hot-reload.md` (status: proposed) scopes a SIGHUP-driven atomic reload of the request-side configuration: matcher, redactor, spotlight, schema, tool gate, and policy index. The design wraps the config-derived subset of `AppState` in `ArcSwap<ReloadableState>` so handlers acquire a per-request snapshot with a single lock-free pointer load. Validation-on-reload is all-or-nothing; on failure the live state is retained and a `reload_failed` audit entry is written. Listening socket, backend HTTP client, budget DB, and the audit file handle remain restart-only. No implementation yet.

### Docs — dependency policy catches up with Cargo.toml

`CLAUDE.md > Dependency Policy` listed only some of the crates the binary actually pulls in. `tower-http`, `serde_json`, and `tracing-subscriber` have been on the dependency list since v0.5 / v0.6 but were missing from the Allowed line; added now so the policy matches the lockfile.

### Docs — internal notes layout cleanup

Tracked artifacts no longer reference unpublished internal notes by filename or path. The CLAUDE.md Documentation Policy now recognises two internal directories, both gitignored: one agents may edit (working notes that reflect the project's current state) and one they may only read (unsettled proposals, strategy, brainstorming). Public files — code comments, `docs/*`, `CHANGELOG.md`, `README.md` — must stand alone; allusions to internal-only material have been stripped.

This commit removes nine such references that had leaked into tracked files (`src/policy/mod.rs`, `src/guard/sse_deanon.rs`, `src/bin/nanoguard-eval.rs`, `docs/design/policy-engine.md`, `docs/research/test-matrix.md`, and two CHANGELOG lines). The content that used to live inline in those references is preserved where it was useful and dropped where it was just a pointer.

### Docs — public design docs for v0.4 / v0.5 features

Three new files under `docs/design/`, all status `shipped`, written so the existing implementations finally have a public design doc to point at:

- **`docs/design/tool-gate.md`** — what Tool Gate inspects (name → schema → entity scan), the three decisions (Allow / Deny / Sanitize), endpoint coverage on `/v1/chat/completions` and `/v1/messages`, the streaming-Sanitize-degrades-to-Allow caveat, and pipeline placement before schema validation.
- **`docs/design/spotlighting.md`** — the three transforms (Datamarking / Delimiting / Encoding), why the system rider is non-optional, why spotlighting runs after PII redaction so placeholders survive, and the explicit "this is not a proof, just a tilt" framing.
- **`docs/design/json-schema.md`** — Draft 2020-12 validator, wrapper-stripping for prose / markdown fences, the three `on_violation` actions (with `Repair` reserved as a forward-compat stub), per-route + per-model rule selection, and the shared validator with Tool Gate.

These were all in `_*.md` notes only; promoting them keeps `docs/` honest with what `src/guard/{tool_gate,spotlight,schema}.rs` already does.

### Changed — release flow is PR-driven

`main` is protected by a ruleset (set up immediately after v0.7.0) that rejects direct pushes. The previous `tools/release.sh` committed straight to `main` and tripped the rule on every release. The flow is now two scripts:

- **`tools/release.sh`** runs `make preflight`, bumps `Cargo.toml` + `Cargo.lock` via `cargo set-version`, promotes `CHANGELOG.md`'s `## [Unreleased]` block to a dated `## [<new>]` section (or prepends a stub if there isn't one), commits on a fresh `release/v<new>` branch, pushes the branch, and opens a PR through `gh pr create`. It never commits to `main` and never tags.
- **`tools/release-tag.sh`** is a separate step. After the release PR has merged, it switches to `main`, fast-forward-pulls, validates that `Cargo.toml` and `CHANGELOG.md` agree on the version, then creates and pushes the `v<new>` annotated tag against the merge commit. Splitting tagging out of the bump step keeps the tag aligned with the merge SHA — even when the PR is squashed.

Both scripts are surfaced through Make: `make release-{patch,minor,major}` for step 1, `make release-tag` for step 2.

### Changed — `make preflight` now requires `cargo-audit`

Previously preflight printed a warning and skipped `cargo audit` when the binary wasn't installed. CI's `security audit` job runs it unconditionally, so skipping it locally meant advisories could surface only after a PR was opened. Preflight now exits with an actionable install hint instead.

Install once per machine:

```bash
cargo install cargo-audit
```

### Changed — agent autonomy on push/PR/merge

The previous rule said "agents must not run `git commit` or `git push` without the user explicitly asking." That made every feature-branch handoff a manual round-trip. The new rule has three tiers:

- **Commit + push on feature branches + `make pr`**: allowed without explicit instruction, after `make preflight` passes.
- **`gh pr merge` (self-merge)**: allowed under tight conditions — only PRs the agent opened in the current session, only with all required CI checks green, only after a diff-vs-description consistency check, and never for release PRs or security-sensitive scope. The full contract lives in `CLAUDE.md > Branch Policy > Self-merge contract`. Default merge method is squash.
- **`make release-tag`**: still gated on the user explicitly confirming the release PR has merged. CI status alone is not a green light for tagging.

Direct pushes to `main` remain disallowed (and are blocked by the ruleset anyway).

### Docs

- `CLAUDE.md > Branch Policy` gains a "Release flow" subsection describing the two-step PR-driven release, and an "Agent autonomy" subsection codifying the new push/PR rule.
- `tools/README.md` rewritten to match the new flow (release.sh = step 1, release-tag.sh = step 2).

## [0.7.0] — 2026-05-11

Policy Engine v1, plus several integration fixes uncovered while stress-testing the e2e suite.

### Fixed

- `SseFilter::try_extract_usage` was bailing on any chunk that contained the literal `[DONE]`, which means streaming responses that pack the `usage` event and the `[DONE]` terminator into one TCP read silently dropped their usage. Walk every `data:` line and only treat a JSON parse with `usage` as a hit. Streaming budget accounting now actually records spend (e2e scenario 17 confirms).
- `SseFilter::try_extract_usage` previously returned `None` the first time it hit a non-`data:` line because of an unwrap chain on `?`. Replaced with `let-else` so unrelated lines are skipped instead of aborting the scan.
- `/v1/messages` previously accepted `stream: true` and tried to JSON-parse the SSE response body, surfacing as an opaque 502. It now refuses early with HTTP 400 and a clear error, matching the README's "streaming not yet supported" note (e2e scenario 19).
- rustdoc warning on `ValidationOutcome::extracted` (a stray code-fence in the doc comment) — reworded to avoid the inline fence.

### Added — Policy Engine

Policy Engine v1: declarative YAML rule bundles with stable rule ids, categories, severities, and compliance tags. Audit log entries gain `rule_id` / `category` / `severity` / `compliance` when a match comes from a policy.

### Added — Policy Engine

- **YAML bundle loader** in `src/policy/`. A bundle is a versioned list of rules, each with a stable `id`, a `category`, a `severity`, an action (`block` / `alert` / `flag` / `redact`), and optional `compliance` tags. Patterns are either literal phrases or regex (`/.../`).
- **Merge into existing matchers** at startup: literal-keyword rules append to `KeywordConfig.inline_block` / `inline_alert` / `inline_flag` based on action; regex `redact` rules contribute entity-named patterns to the redactor. The matcher / redactor hot path is untouched.
- **`PolicyRuleIndex`** — a lookup table from matched literal text or regex body to rule metadata. Built once at startup and stored on `AppState`.

### Added — Audit log enrichment

- `AuditEntry` gains four optional fields: `rule_id`, `category`, `severity`, and `compliance`. They are omitted from the JSON when absent (backward compatible).
- The audit writer consults `PolicyRuleIndex` whenever there's a `matched_rule`, including matches demoted by shadow mode (the `shadow_block:` prefix is stripped before lookup).

### Configuration

```toml
[policies]
bundle_path = "policies/default.yaml"
```

```yaml
# policies/default.yaml
version: 1
metadata:
  name: nanoguard default
  updated: "2026-05-10"

rules:
  - id: PI-001
    category: prompt_injection
    severity: high
    pattern: ignore previous instructions
    action: block

  - id: PII-001
    category: pii
    severity: medium
    pattern: '/[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}/'
    action: redact
    placeholder: EMAIL
    compliance: ["GDPR", "HIPAA"]
```

### Tests

- 140 unit tests in total. New: 11 in `src/policy/` (parsing, validation, dispatch, lookups), 2 in `src/proxy/sse.rs` (usage extraction with `[DONE]` packed in the same chunk).
- e2e suite extended to **19 scenarios / 41 assertions**. New / strengthened scenarios:
  - **13**: strengthened from a single PII round-trip into three sub-scenarios — 13a re-checks redaction round-trip with tools enabled, 13b verifies the Anthropic adapter actually converts upstream `tool_calls` into `content[].type == "tool_use"` blocks (the conversion path that scenario 12 does not exercise), and 13c verifies a denied tool call is removed from the response and surfaced as a `nanoguard_denied_tools` block.
  - **15**: policy bundle audit enrichment — verifies that an audit entry from a policy match carries `rule_id` / `category` / `severity`.
  - **16**: streaming tool gate deny path — confirms `tool_call_denied` event is emitted and the stream terminates with `[DONE]` when a denied call is assembled from deltas.
  - **17**: streaming budget accounting — confirms `stream_options.include_usage` chunks reach the budget store (queried via `/v1/admin/budget/:api_key`).
  - **18**: schema reject mode — confirms `on_violation = "reject"` returns 4xx instead of just logging.
  - **19**: Anthropic streaming refusal — confirms `stream: true` on `/v1/messages` is refused cleanly with a 400 instead of half-handled.
- `policies/default.yaml` ships with the repo so deployments can copy and edit it.

### Out of scope for this release

- Hot reload / signed bundles (Phase 5.4 follow-ups).
- Migrating Spotlight / Tool Gate / Schema configurations into the policy file (still TOML).
- Multi-bundle stacking and per-tenant override hierarchies.
- A literal pattern with `redact` / `reject` / `log` action — for those, declare a regex pattern with a placeholder.

These are intentional gaps; see the "Limitations" section in `docs/design/policy-engine.md`.

## [0.6.0] — 2026-05-10

Three follow-ups that finish the Phase 4 trio (Spotlighting + Schema + Tool Gate) on every supported endpoint, plus a recognizer evaluation harness for tuning dictionary packs.

### Added — Anthropic parity

- `/v1/messages` now routes responses through Tool Gate and Schema Validator. Denied tool calls drop out of the response; surviving calls become `{"type":"tool_use", ...}` blocks in the Anthropic content array. A `nanoguard_denied_tools` block is appended when anything was rejected.
- `/v1/messages` now applies Spotlighting (datamarking / delimiting / encoding) to untrusted-role messages after the Anthropic→OpenAI normalization step, matching the behavior of `/v1/chat/completions`.
- Schema rules can now target `/v1/messages` (in addition to `/v1/chat/completions`) via `[[output.schema.rules]] endpoint = "/v1/messages"`.

### Added — Streaming Tool Gate

- `src/guard/sse_tool_gate.rs` — new accumulator that walks `choices[].delta.tool_calls[]` events, reassembles partial tool calls keyed by index, and feeds the completed call to the existing `ToolGate` once `finish_reason: "tool_calls"` arrives.
- On a Deny outcome the proxy emits a synthetic `data: {"error":{"type":"tool_call_denied", ...}}` event followed by `data: [DONE]`, terminating the stream so the client cannot execute a denied tool.
- `Sanitize` is degraded to `Allow` in the streaming path: by the time the full arguments are visible, the delta chunks carrying those arguments have already been forwarded. Sanitize stays available on the non-streaming path. Documented in the module header.

### Added — Recognizer evaluation harness (`nanoguard-eval`)

- New binary `nanoguard-eval` (under `src/bin/nanoguard-eval.rs`) runs the request-side `Redactor` over a labeled JSONL corpus and reports per-entity Precision / Recall / F1 plus the totals.
- Surfaces the top false positives (detected but not in the gold set) and top false negatives (missed annotations) for diagnostic use.
- Two match modes: `strict` (exact start/end + entity match) and `lenient` (overlap; default).
- Optional `--json <path>` writes a structured report for CI gating; the exit code is non-zero when at least one entity has F1=0 with annotations present.
- New public API `Redactor::find_matches(text) -> Vec<RedactMatch>` exposes match positions + entity names, which the harness consumes and which downstream tooling (audit metadata, decision-id schemas) can also use.

### Tests

- 132 unit tests in total. New: 5 in `sse_tool_gate.rs` (delta accumulation across multi-event tool calls, deny event format, multi-tool responses, no-op events).
- e2e suite extended from 26 to 29 assertions across 14 scenarios. New: scenario 13 verifies Anthropic redaction still works with the tool gate enabled; scenario 14 invokes `nanoguard-eval` against a tiny corpus.

### Notes

- Streaming sanitize remains a deliberate gap. A future iteration could buffer the entire tool-call delta sequence before forwarding any of it, at the cost of streaming latency.
- The eval harness reads a flat JSONL corpus today. Entity-name aliasing (e.g. mapping Presidio's `EMAIL_ADDRESS` to nanoguard's `EMAIL`) is not yet implemented; corpora must use nanoguard entity names directly.
- Tool Gate still does not cover Anthropic streaming responses; that is a follow-up once Anthropic streaming becomes a primary deployment path.

## [0.5.0] — 2026-05-10

JSON Schema validation on responses + a Tool Gate that inspects every LLM-emitted tool call before the application executes it. Plus tooling: `tools/release.sh` / `tools/push.sh`, an end-to-end suite that boots a mock backend internally, and a documentation policy in CLAUDE.md.

### Added — JSON Schema validation (output)

- **`SchemaValidator`** in `src/guard/schema.rs` compiles per-route / per-model JSON Schemas (Draft 2020-12 via the `jsonschema` crate) and validates the assistant's response against the picked rule.
- **Wrapper stripping**: leading prose and markdown fences (` ```json ... ``` `) are removed before parsing so the validator sees plain JSON.
- **Three violation actions**: `reject` (502 to client), `log` (audit only, default), `repair` (reserved name; falls back to log with a warning until a port of `json_repair` lands).
- **Per-route rules**: `[[output.schema.rules]]` entries match `(endpoint, model_pattern, schema_path)` so different routes / models can require different shapes.

### Added — Tool Gate

- **`ToolGate`** in `src/guard/tool_gate.rs` evaluates every OpenAI `tool_calls[]` and Anthropic `tool_use` block emitted by the LLM. Three layered checks:
  1. **Allow / deny by name** with `*` wildcard prefixes/suffixes. Allow list closes the world; deny list always wins.
  2. **JSON Schema validation** of the tool's arguments using the same `jsonschema` crate as the output validator.
  3. **PII / secret scan** on the argument JSON via the existing `Redactor`. Configured `reject_entities` deny the call; `mask_entities` route through `Sanitize { redacted_args }`.
- **Three decisions** flow back to the proxy: `Allow`, `Deny { reason }`, `Sanitize { redacted_args }`. Denied tool calls are removed from `tool_calls` and surfaced under `message.nanoguard_denied_tools` so the client can react.
- **Streaming pass-through**: tool gate currently runs only on non-streaming responses. Streaming tool-call detection (delta accumulation + `finish_reason: "tool_calls"` evaluation) is a follow-up.

### Added — Tooling

- **`tools/release.sh`** automates `cargo set-version` (cargo-edit) → `cargo test` → `tools/e2e.sh` → commit → tag → optional push, with `patch` / `minor` / `major` / explicit-version arguments and `--no-push` / `--skip-e2e` / `--dry-run` flags.
- **`tools/push.sh`** publishes `main` and any locally-existing tags reachable from `HEAD` that are not yet on origin. Refuses with a dirty working tree.
- **`Makefile`** gains `release-patch` / `release-minor` / `release-major` / `push` targets.
- **`make e2e`** now drives `tools/e2e.sh`, which boots the mock backend and a release nanoguard binary internally — no servers need to be running first. The previous pre-running-server flow is preserved as `make e2e-live`.

### Configuration additions

```toml
[output.schema]
enabled = false
on_violation = "log"            # "reject" | "log" | "repair"

[[output.schema.rules]]
endpoint = "/v1/chat/completions"
model_pattern = "gpt-4o.*"
schema_path = "schemas/user_card.json"
name = "user_card"

[tools]
enabled = false
allow = ["search_*", "read_*"]
deny  = ["delete_*", "shell_exec"]
reject_entities = ["AWS_ACCESS_KEY_ID", "JWT"]
mask_entities = ["EMAIL"]

[[tools.schemas]]
tool_name = "send_email"
schema_path = "schemas/send_email.json"
```

### Documentation

- **CLAUDE.md** gains a Documentation Policy section requiring every file under `docs/design/` and `docs/research/` to start with a status marker (`shipped` / `partial` / `proposed` / `deprecated`), and a code-doc sync contract.
- **CLAUDE.md** dependency list refreshed to match the actual Cargo.toml; module table reflects current `src/` layout (proxy, matcher, budget, audit, admin, config, guard reserved for higher-level pipeline components).

### Tests

- 21 new unit tests: `src/guard/schema.rs` (9), `src/guard/tool_gate.rs` (12).
- e2e suite extended from 22 to 26 assertions across 12 scenarios. New: output schema log-only violation (11), tool gate deny / allow / sanitize round-trip (12).
- `tools/mock_backend.py` gained a `TOOL:` hook so a test can deterministically request that the mock emit a specific `tool_calls` payload.
- `tools/e2e.sh` now uses `jq -n` to construct request bodies, which fixes a shell-quoting bug that bit scenario 12 during development.

### Notes

- All new features are opt-in. Existing deployments (`reversible = false`, `tools.enabled = false`, `output.schema.enabled = false`) are unaffected.
- Tool Gate runs after Vault deanonymize, so PII placeholders set on input have already been resolved by the time tool arguments are scanned. A future iteration may move the scan earlier to also catch secrets the LLM hallucinates into arguments before deanonymize completes.
- Anthropic `/v1/messages` does not yet route through Tool Gate (its tool-result shape warrants a separate pass).

## [0.4.0] — 2026-05-10

Indirect prompt injection defense via Spotlighting. Untrusted message content (RAG chunks delivered in `tool` / `function` role messages) is wrapped or transformed so the LLM treats it as data rather than instructions.

### Added — Spotlighting

- **Three transforms** in `src/guard/spotlight.rs`:
  - `datamarking` (default): replace ASCII whitespace inside untrusted content with a marker character (`^`), making the region visually distinguishable as preprocessed data.
  - `delimiting`: wrap content with configurable open/close markers (`<<UNTRUSTED>>`...`<</UNTRUSTED>>` by default).
  - `encoding`: base64-encode content. Strongest isolation, lowest response quality — opt-in.
- **Automatic system rider** explaining the convention to the model is appended to the existing system message (or prepended as a fresh system message if none exists). Without the rider the wrapping is security theater.
- **`untrusted_roles` is configurable** — defaults to `["tool"]`, can be extended to `["tool", "function"]` etc.
- **Order in the input pipeline**: spotlighting runs *after* PII redaction, so placeholders are already in place and are not mangled by datamarking. Spotlighting touches only roles in `untrusted_roles`; user / system / assistant content passes through.

### Configuration

```toml
[input.spotlight]
enabled = false                # opt-in
method = "datamarking"         # "datamarking" | "delimiting" | "encoding"
untrusted_roles = ["tool"]
delimiter_open = "<<UNTRUSTED>>"
delimiter_close = "<</UNTRUSTED>>"
datamark_char = "^"
# system_rider = "..."         # override the default rider per method
```

### Tests

- 9 unit tests covering each transform, rider injection (existing system / new system), parts-array text, and custom untrusted-role lists.
- e2e scenario 10 in `tools/e2e.sh` validates that `tool` role content is datamarked, the rider is injected, and the user message is left untouched.
- Total: 101 lib tests + 22 e2e assertions across 10 scenarios.

### Notes

- Spotlighting is purely a request-side preprocessor. It does not affect the response path or streaming.
- Anthropic `/v1/messages` does *not* yet apply spotlighting (its tool-result message shape differs and warrants a separate pass).
- A RAG chunk pipeline that builds on spotlighting is on the roadmap.

## [0.3.0] — 2026-05-10

This release turns nanoguard from a keyword/regex proxy into a full PII-aware AI security gateway with reversible redaction, streaming-aware filtering, and per-entity policy controls. All additions are backward compatible — existing `nanoguard.toml` configs continue to work unchanged.

### Added — PII redaction subsystem

- **Entity-named Redactor** (`src/proxy/redact.rs`) replaces the four hardcoded regexes with 13 built-in entities (EMAIL, SSN, CARD, AWS_ACCESS_KEY_ID, GITHUB_PAT, GITHUB_FINE_GRAINED_PAT, OPENAI_KEY, ANTHROPIC_KEY, STRIPE_KEY, GOOGLE_API_KEY, JWT, SLACK_TOKEN, TOKEN). Custom dictionaries are loaded via `input.pii.dict_paths` in `/regex/<TAB>ENTITY_NAME` form.
- **Per-entity action overrides** through `[input.pii.entities]`: e.g. `AWS_ACCESS_KEY_ID = "reject"`, `EMAIL = "mask"`, `PHONE = "log"`. Reject always wins — any reject-class match short-circuits before mask runs.
- **Indexed placeholder styles** via `placeholder_style`: `bare` (`[EMAIL]`), `indexed` (`[EMAIL_1]`), or `llm_guard` (`[REDACTED_EMAIL_1]`). Indexed forms preserve the distinction between multiple values of the same entity type within one prompt.
- **Reversible Vault-backed redaction** (`[input.pii] reversible = true`): a per-request Vault stores `(placeholder, original)` pairs at input time and restores them in the response, so the model never sees raw PII while the client receives unmasked output.
- **Anthropic `/v1/messages` redaction**: previously only `/v1/chat/completions` applied PII protection; both endpoints now share the same logic and both Anthropic content shapes (string and `Blocks`) are walked.

### Added — Streaming SSE pipeline

- **Buffered SSE filter** (`src/proxy/sse.rs`): chunks are accumulated and split on the SSE blank-line terminator, so events that span TCP chunk boundaries or multiple events packed into one chunk are handled correctly. Non-data lines (`event:`, comments, `[DONE]`) pass through untouched.
- **Streaming deanonymizer** (`src/guard/sse_deanon.rs`): a state machine buffers from `[` to `]` so a placeholder split across SSE events (e.g. `[REDACT` in event 2, `ED_EMAIL_1]` in event 3) is still resolved against the per-request Vault.
- **Streaming budget accounting**: when clients enable OpenAI's `stream_options: {include_usage: true}`, the final chunk's `usage` block is captured and recorded against the budget store after stream completion.

### Added — Obfuscation-resistant input normalization

- **NFKC Unicode normalization** (default on): full-width letters such as `ｊａｉｌｂｒｅａｋ` fold to ASCII before scanning. ASCII fast path skips NFKC entirely for pure-ASCII input, keeping the hot-path under 250 ns.
- **Zero-width character stripping** (default on): strips `U+200B`, `U+200C`, `U+200D`, `U+2060`, `U+FEFF`.
- **Separator collapse** (`separators = true`, opt-in): `j-a-i-l-b-r-e-a-k` → `jailbreak`. A length-≥4-letter run threshold preserves common dashed words like `co-op` and `e-mail`.
- **Leet-speak fold** (`leet = true`, opt-in): `j41lbr34k` → `jailbreak`. Disabled by default to avoid false positives on text like `3D printer`.

### Added — Operational features

- **Shadow mode** (`[input] shadow = true`): scan and audit every request without enforcing. Blocked verdicts are demoted to Flagged with a `shadow_block:` prefix on the matched rule, useful for measuring false-positive rates of new rules before turning them on.
- **Industry policy packs** (`dicts/policies/`): opt-in keyword bundles for healthcare (HIPAA-aware PHI), finance (PCI-DSS / GLBA / MNPI), and legal (attorney-client privilege). Add the file paths to `input.keyword.dict_paths` to enable.
- **Japanese keyword coverage** in the default `dicts/pii.txt` (マイナンバー, パスワード, 銀行口座, 健康保険証, etc.) and in each industry pack.

### Added — Module structure

- New top-level module `src/guard/` reserved for higher-level guard pipeline components (vault, deanonymize, sse_deanon today; planned: tool gate, spotlighting, schema enforcement).
- New end-to-end test scaffolding under `tools/` (`mock_backend.py`, `e2e.sh`, `e2e.toml`) covering 19 assertions across 9 scenarios. Run with `./tools/e2e.sh` after a release build.

### Changed

- Default keyword engine is now **aho-corasick** (`engine = "aho-corasick"`); the older `iword-rs` engine remains available via config.
- README has been rewritten to be HN-appropriate: honest claims, edge AI framing, no unsubstantiated comparison numbers, explicit `Limitations` section.
- CLAUDE.md gains a Documentation Policy section: every file under `docs/design/` and `docs/research/` must carry a status marker (`shipped` / `partial` / `proposed` / `deprecated`).

### Notes for upgraders

- No breaking changes. `reversible` defaults to `false`, so existing deployments continue to mask without round-trip.
- Recommended path to enable Vault round-trip:
  ```toml
  [input.pii]
  reversible = true
  placeholder_style = "indexed"   # auto-promoted from "bare" if reversible is on
  deanonymize_strategy = "exact"  # "case_insensitive" also available
  ```
- `deanonymize_strategy = "fuzzy"` and `"combined"` are reserved names that currently fall back to `exact` with a warning.

## [0.2.0] — earlier

Initial pluggable matcher engine, audit logging (JSONL + SHA-256 hash-only), graceful shutdown, and admin budget API. See git history for details.
