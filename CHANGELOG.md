# Changelog

All notable changes to nanoguard are documented in this file. The format is loosely based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
