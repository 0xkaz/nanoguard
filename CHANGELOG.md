# Changelog

All notable changes to nanoguard are documented in this file. The format is loosely based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added — Console Users tab: edit `allowed_models` and reset another user's password

The Users tab gains an **Edit User** dialog reachable from each row's existing "Edit" button (which had no handler before — clicking it was a no-op). The dialog edits two fields that were previously TOML- or CLI-only:

- **`allowed_models`** — comma-separated input that is serialised back into the JSON-array string the DB stores. Empty input means "no restriction" (matches the column's `'[]'` default). The `UpdateUserRequest` field had been present on the wire since the schema landed, but no surface exercised it; the row showed up in `/api/users` responses and was readable only via raw SQL.
- **Reset password** — a second section in the same dialog. Replaces `users.password_hash` and runs `delete_user_sessions(target.user_id)` in **one SQLite transaction** so a crash mid-call cannot leave the new credential live while a compromised session cookie keeps working. The handler mirrors `nanoguard-admin set-password` byte-for-byte (same hash function, same sweep, same atomicity); the CLI was the only way to do this before, and it required shell access to the host.

A new endpoint `POST /api/users/:id/reset-password` (admin-only, CSRF-rotated) backs the reset action. Audited as `user_password_reset` with `target = <target username>`; the body deliberately omits the new password from the audit record. Validation: `password.len() >= 12` and not on the common-password list — the same two gates `api_create_user` enforces.

**e2e** — scenario 42 covers both features with 10 assertions: `allowed_models` edit persists and is visible in `/api/users`, target user can log in with the original password (baseline), reset endpoint returns 200 with the target username echoed, old password is rejected after reset (401), new password works (200), the target's pre-reset session cookie is invalidated (401 on `/api/me` — verifies the in-transaction session sweep), and a non-admin caller is 403 on the reset endpoint. Total `tools/e2e.sh` assertions: 219 (was 209 after PR #47).

### Changed — Client-auth Stage 2 (slice 2): Anthropic `/v1/messages` budget wiring

`/v1/messages` joins `/v1/chat/completions` on the `ClientView.budget_key` contract. With `[auth].enabled = true`, Anthropic requests are accounted against `token:<id>` derived from the verifier middleware — the same bucket the OpenAI path uses, so a single token's quota covers both endpoints. A Claude SDK caller with a leaked token can no longer rack up unaccounted Anthropic spend, which was the asymmetry slice 1 left open.

Budget check runs after spotlighting and before backend resolution; an over-quota call returns `429` with an Anthropic-shaped envelope (`{ type: "error", error: { type: "rate_limit_error", message } }`) so Claude SDKs read it the same way they read an upstream Anthropic 429. Successful calls record spend against the OpenAI-shape `prompt_tokens` / `completion_tokens` returned by the backend (backends always speak OpenAI on the wire; Anthropic mode is a shape adapter, not a parallel transport).

When `[auth].enabled = false`, Anthropic budget falls back to the literal `default` bucket. Anthropic's request body has no OpenAI-style `user` field to fall back to, so the fallback is constant rather than self-asserted — symmetric with the chat-completions fallback when `user` is absent from the body.

Out of scope for this slice (still tracked in [`docs/design/client-auth.md`](docs/design/client-auth.md)): `stream=true` on `/v1/messages` (still refused with 400), audit `user_id` / `token_id` columns, `last_used_at` flush, and shadow mode.

**e2e** — scenario 41 covers the new contract with 7 assertions: authed `/v1/messages` returns Anthropic-shape, spend lands under `token:<id>`, legacy `default` bucket stays untouched when authed, per-token limit on `/v1/messages` returns `429` with `type=error` + `error.type=rate_limit_error`, and with `[auth].enabled = false` the constant `default` bucket records spend. Total `tools/e2e.sh` assertions: 209 (was 202 after PR #46).

### Added — Console Backends tab: edit `[routing]` from the UI

The Backends tab gains a Routing section that edits `[routing]` without dropping into the Config tab's raw TOML editor. Operators can change the default backend, add / delete rules, and reorder them with ↑ / ↓ buttons (the order is semantically meaningful — `[routing].rules` is first-match-wins). Save goes through a single `PUT /api/routing` round-trip that validates, atomically rewrites the `[routing]` section via `toml_edit` (every other section, comment, and key ordering preserved verbatim), and fires a reload — `[routing]` is hot-reloadable, so the change takes effect on the next request without a restart.

Validation rejects: empty `default`, `default` not in `[backends.*]`, any rule whose `backend` is not in `[backends.*]`, empty `model`, and duplicate `model` patterns (the later occurrence could never fire under first-match-wins; almost always a typo).

Mutations are admin-only, CSRF-rotated, and audited as `routing_update` with target `routing` and an `after` payload carrying the new default + rules. `restart_required: false` rides on the success response so the SPA can show the right toast.

**e2e** — scenario 40 covers the new endpoint with 10 assertions: pre-change baseline (premium → default backend), `PUT /api/routing` echoes default + rule count + `restart_required: false`, the new rule lands in `nanoguard.toml` on disk, hot reload picks it up (premium now hits the other backend), unknown `default` rejected (400), unknown `rule.backend` rejected (400), duplicate `model` rejected (400), viewer 403. Total `tools/e2e.sh` assertions: 202 (was 192 after PR #45).

### Changed — Client-auth Stage 2 (slice 1): per-token budget buckets

When `[auth].enabled = true`, the budget bucket for `/v1/chat/completions` switches from the request body's self-asserted OpenAI `user` field to a verified `token:<id>` derived from the `ClientView` the verifier middleware attaches. A runaway script under one token no longer depletes a sibling token belonging to the same user, and a malicious caller can no longer squat on a victim's bucket by setting `"user": "victim"` in the request body. The body's `user` field is still forwarded to the upstream backend unchanged — it remains a backend-side usage tag, not a nanoguard policy decision.

When `[auth].enabled = false`, the legacy body-`user`/`default` bucket is preserved verbatim so existing single-tenant deployments continue to work without config changes.

`ClientView` gains a `budget_key: String` slot set during verification. `/v1/chat/completions` reads it via `Option<Extension<ClientView>>` and falls back to the legacy `extract_api_key(&body)` path only when no view is attached. Admin endpoints (`GET|PUT|DELETE /v1/admin/budget/:api_key`) accept the new `token:<id>` form because the path extractor is a free string — no schema migration is needed.

Out of scope for this slice (tracked in [`docs/design/client-auth.md`](docs/design/client-auth.md)): Anthropic's `/v1/messages` still bypasses budget accounting entirely, the `user:<id>` aggregate admin form, audit `user_id` / `token_id` columns, `last_used_at` flush, and shadow mode. Each of those is its own Stage 2 slice.

**e2e** — scenario 39 covers the new contract with 6 assertions: authed request succeeds, spend lands under `token:<id>`, the body's `user` field does **not** create a separate bucket (the squat scenario), the legacy `default` bucket stays untouched when `[auth].enabled = true`, the per-token limit applied to `token:<id>` is enforced (429), and with `[auth].enabled = false` the legacy body-`user` budgeting is preserved. Total `tools/e2e.sh` assertions: 192 (was 186 after PR #44).

### Added — Console Playground tab: query proxy and raw backend side by side

New admin-only **Playground** tab in the Web Configuration UI: paste an OpenAI chat-completions request body, pick a backend from the dropdown, and click either "Send through proxy" (= full guardrail pipeline via `http://<listen>/v1/chat/completions`) or "Send direct to backend" (= `<backend.endpoint>/v1/chat/completions` with no guardrails). Both responses render side by side with their upstream HTTP status and round-trip latency, so an operator can compare "what guardrails did" against "what the backend would have answered". Two new endpoints back the tab: `POST /api/playground/proxy` and `POST /api/playground/backend`. Both are admin-only, both rotate CSRF on every call, and the backend-direction call reads `api_key` from the live `[backends.*]` config rather than trusting the caller — admins cannot exfiltrate keys or aim the call at an arbitrary URL through the playground.

Playground calls are audited under `playground_proxy` / `playground_backend` / `playground_error` in `console-audit.jsonl` with `target = proxy | backend:<provider>`. The **request body is intentionally not logged** because operators routinely paste secrets while testing redaction; the audit trail captures who ran the call, not the prompt.

**e2e** — scenario 38 covers the new endpoints with 10 assertions: clean prompt forwards 200 through proxy and through the backend-direct call, the response carries a `where` discriminator (`proxy` / `backend:<provider>`) and a `latency_ms` integer, a keyword-trapped prompt is 400 on the proxy path but still 200 on the backend-direct path (the divergence is the whole point of the tab), an unknown backend label returns 404, and a non-admin viewer is 403 on both endpoints. Total `tools/e2e.sh` assertions: 186 (was 169 after PR #43).

### Changed — single-process boot; `nanoguard-console` binary retired

The `nanoguard` binary now spawns the Web Configuration UI listener inline when `[console].enabled = true` (the new default). A first-time operator types `make run` and gets both the proxy on `:8080` and the console on `:8081` from a single command. Set `[console].enabled = false` in `nanoguard.toml` for headless deployments where the management UI is undesired.

The standalone `nanoguard-console` binary is **retired** — single-process is the only supported boot mode. The `[[bin]] name = "nanoguard-console"` entry, `make run-console` target, and the matching `$CONSOLE_BIN` plumbing in `tools/e2e.sh` are gone. `make run` now folds in the bootstrap-admin-password convenience the retired target used to provide (auto-generate when the users table is empty, print once, recognise an existing admin without overwriting), so operators who used `make run-console` for that UX lose nothing.

`make run` accepts `CONSOLE_SESSION_SECRET` and `BOOTSTRAP_PASSWORD` from the shell environment as before; when unset, the target generates ephemeral values and prints the admin password exactly once on first boot. The `BOOTSTRAP_PASSWORD` env-slot remains visible in `/proc/<pid>/environ` until the operator `unset`s it after first run — the in-memory `Zeroizing<String>` wrap kills the heap copy but not the env image. Documented in README's "Recovering a forgotten admin password" section.

`src/main.rs` drops `#[tokio::main]` and constructs the runtime by hand so the bootstrap password can be read pre-runtime. The new `console::prepare_for_run` helper centralises `CONSOLE_SESSION_SECRET` env override, secret-length validation, and the pre-runtime `BOOTSTRAP_PASSWORD` read — keeping the path in one place after the standalone binary's removal.

**Fail-fast on console bind error.** When `[console].enabled = true` and `console::run` exits early (bind error, panic, anything), `nanoguard` treats that as fatal and shuts the proxy down too. A partial boot — proxy up, console silently dead — was exactly the state we want to fail loud on. `tokio::select!` races the proxy server against the console join handle; whichever exits first decides the process outcome.

**e2e** — scenario 37 covers the new contract with 7 assertions (proxy + console on the same PID, in-process spawn log line, `[console].enabled = false` suppresses the listener cleanly, proxy still serves `/health` in disabled mode). Scenarios 26–36 unified on the single binary: every `[console]` block carries `enabled = true`, the secondary `$CONSOLE_BIN` invocations are gone, the new `kill_leftover_nanoguards` helper sweeps any zombie from a flaky previous tear-down, and scenarios 28 / 36 switch their reload trigger from `[reload].socket` to `[reload].pid_file` because same-runtime socket reads under the new boot model have a race window that SIGHUP does not.

### Added — Multi-backend routing: one proxy, many upstreams

A single proxy can now hold N upstream backends (OpenAI, Anthropic, Ollama, DeepSeek, …) side-by-side and dispatch each request to the right one based on the request body's `model` field. Phase 1 of `docs/design/multi-backend-routing.md` ships in this release; per-client `allowed_models`, audit verdict expansion (`model_denied` / `model_unrouted`), and provider-side failover remain proposed.

**Schema.** `[backends.NAME]` (operator-chosen label) declares one backend per section, with the existing `provider` / `endpoint` / `api_key` / `model` fields. `[routing]` carries `default = "<name>"` and `rules = [{ model = "...", backend = "..." }]`. Patterns support exact strings and trailing-`*` globs (`gpt-4o-*` matches `gpt-4o-mini`); rules are scanned in declared order, first match wins. The legacy single `[backend]` section still works — nanoguard synthesizes a `default` pool entry from it, and a startup warning fires if both schemas are present.

**Backend pool.** `Backend` instances keep their per-backend `reqwest` connection pools (one per upstream) and are preserved across hot reload, because orphaning a connection pool mid-request is unsafe. Adding or removing entries in `[backends.*]` is therefore restart-only; the `warn_on_restart_only_drift` helper reports the change on SIGHUP. `[routing]` (the rules + default) IS hot-reloadable — the proxy picks up new routing immediately on reload.

**Proxy hot path.** Both `/v1/chat/completions` and `/v1/messages` extract the request body's `model`, call `state.pool.route(model)`, and forward through the resolved backend. An unroutable model returns 400 with a clear error rather than reaching some default upstream silently. `/v1/models` continues to surface the legacy default backend's list for compatibility; per-backend model federation is a future iteration.

**Console (`nanoguard-console`).** A new admin-only **Backends** tab lists every configured backend, shows the routing default, and offers Add / Edit / Delete buttons that go through the new `GET|POST /api/backends` and `PUT|DELETE /api/backends/:name` endpoints. Each mutation rewrites `nanoguard.toml` via `toml_edit` so unrelated sections, comments, and whitespace stay verbatim. Deletes are refused with 409 when the target is the routing default or referenced by any `[routing]` rule. Every mutation goes through an atomic-write + backup + reload trigger, audited under `backend_create` / `backend_update` / `backend_delete` in `console-audit.jsonl`. The response carries `restart_required: true` so the UI can warn the operator that the live pool only picks up the change on the next process restart.

**Overview API.** `/api/overview` now returns a `backends` array (one entry per pool member, with `name`, `provider`, `endpoint`, `model`, `is_default`, `has_api_key`) plus a `routing` object containing `default` and the rules list. The legacy `backend` key on the response keeps the first entry for old SPA builds.

**e2e.** Two new scenarios:
- **35** spawns two labelled mock backends on different ports, sets `[routing]` rules for `premium` (exact → `alpha`) and `fast-*` (glob → `beta`), and confirms each request reaches the right mock. Also: unmatched `model` falls back to `[routing].default`; `/api/overview` reports both backends and the rule list.
- **36** drives the Console Backends API end-to-end: list shows the bootstrap entry, POST creates a new one with `restart_required: true` and the new section appears in `nanoguard.toml`, list now shows 2, duplicate POST is 409, DELETE drops the row, deleting the routing default is 409, viewer (non-admin) is 403 on every endpoint.

`tools/e2e.sh` is now at 169 assertions (was 147 at PR #41 merge).

CodeRabbit review fixes applied during the PR (each closes a real defect, not a stylistic nit):

- **`[routing].default` is strictly required when N>1 backends are configured.** The earlier "pick BTreeMap-first and warn" path silently routed unmatched models to whichever backend sorted alphabetically first; a typo'd model name leaked to an unrelated upstream. `Config::pool()` now refuses to start in that state.
- **`api_key` field is 3-state in the Console PUT body.** Previously, omitting `api_key` cleared the stored key — exactly the path an operator who hit Save without retyping their secret would take. The field is now `Option<Option<String>>` via a double-Option deserializer: omitted = keep current, explicit null = clear, string = replace. `upsert_backend_in_toml` preserves the rest of the existing entry instead of doing a full overwrite. SPA UI updated with a matching three-choice prompt.
- **Reload validates `[routing]` against the LIVE pool, not the new TOML.** Because `[backends.*]` is restart-only, a SIGHUP that added a rule pointing at a not-yet-restarted backend used to graft an invalid route onto the live state and silently 400 every matching request. `reload_once` now refuses the swap with a clear `reload_failed` reason.
- **Routing-miss errors are OpenAI/Anthropic-shaped.** `/v1/chat/completions` now returns the `{error: {message, type, code, param}}` envelope; `/v1/messages` returns `error.type = "invalid_request_error"` matching the Anthropic spec for client-side 400s.
- **`/api/overview` legacy `backend` field comes from `routing.default`,** not `backends.first()`. Old SPA builds that only read the legacy key now see the active default upstream instead of an alphabetical accident.
- **`/v1/models` aggregates the full pool.** Queries every backend in parallel, tags each model with `owned_by = <backend label>`, fails soft on per-backend errors (returns 502 only when every upstream fails), surfaces partial failures under `partial_errors`.
- **`backend_changed` is migration-aware.** Removing legacy `[backend]` in favor of `[backends.*]` no longer fires spurious "[backend].provider changed" drift warnings on SIGHUP.
- **`docs/design/multi-backend-routing.md`** opens with a "Shipped vs Proposed" call-out so a reader sees Phase 1 boundaries before scrolling into Phase 2 / proposed material.

**Migration.** Existing single-`[backend]` deployments keep working as-is; no migration is required. To move a deployment to multi-backend, add a `[backends.NAME]` section and remove the legacy `[backend]` block (or leave it — the startup warning is informational).

### Added — Console Overview "Getting Started" + "Guards Active" panels, Console-side budget editor

The Console's first screen after login used to be three read-only cards (config file count, audit entry presence, budget key count). An operator who just installed nanoguard had no way to learn from the UI **how to send their first request** — there was no proxy URL displayed, no curl example, no Bearer-wiring hint, and no signal about which guards were active. This release fills those gaps:

- **`GET /api/overview` (new)** — single read-only snapshot of the live proxy: `proxy_url` (with `0.0.0.0:*` normalized to `localhost:*` for copy-paste), `auth` mode and `env_marker`, the configured backend, the documented endpoint set (`/v1/chat/completions`, `/v1/messages`, `/v1/models`, `/health`), and a guard digest (input keyword block / PII / spotlighting / output PII / schema / tool gate / policy bundle, each with `enabled` + a short `summary`). Also surfaces the caller's `user_token_count` so the SPA can tell them whether they have a usable token.
- **Overview "Getting Started" panel** — renders the live proxy URL, a working curl example tailored to whether `[auth]` is enabled, and OpenAI-SDK env-var snippets so a user can paste `OPENAI_BASE_URL=http://localhost:8080/v1` into their app. Inline links jump to the Tokens tab.
- **Overview "Guards Active" panel** — renders the guard digest with green/grey dots so the operator can see at a glance what is on and what is off, plus a hint pointing at the Config tab for changes.
- **Config tab captions** — each file under `nanoguard.toml` / `dicts/*.txt` / `policies/*.yaml` now carries a one-line plain-English description above the content ("what does this file control"). Previously the SPA dumped raw file content with no guidance.
- **Budget tab inline editor (admin-only)** — per-row "Edit limit" and "Reset usage" buttons fire `POST /api/budget/limit` and `POST /api/budget/reset` respectively. Both write the same `api_key_limits` / `api_key_usage` tables the proxy admin API writes, so an operator who only has Console session does not also need an `ADMIN_API_KEY` to manage caps. Both endpoints rotate CSRF, gate on `require_admin`, and record a `budget_set_limit` / `budget_reset_usage` line in `console-audit.jsonl` so changes have a paper trail.
- **README "Point your app at the proxy" section** — adds step 4 in the Web Configuration UI walkthrough with the same curl + SDK form the Overview panel renders, so a reader who has not opened the Console yet still has a working snippet.
- **e2e scenario 34** — covers the new surface end-to-end: `/api/overview` shape (URL normalization, backend digest, guard count, PII enabled flag, endpoint list), the budget limit set/clear/persist round-trip via SQLite, the usage reset round-trip, and the admin-only gate (viewer is 403 on `/api/budget/limit`).

`tools/e2e.sh` is now at 147 assertions (was 134 at PR #40 merge). The added cases pin: (a) the Overview's child guard badges (`Input PII redaction`, `Spotlighting`, `Output PII redaction`, `Output schema validation`) are gated by the parent `[input].enabled` / `[output].enabled` master switch — the pipeline-disabled state surfaces honestly in the UI; (b) setting a budget limit for a never-used api_key still makes it appear in the admin `/api/budget` reader (the handler now upserts a zero-usage row alongside the limit so the JOIN-from-usage reader sees it); (c) `/api/budget/limit` and `/api/budget/reset` rotate the per-session CSRF token on every mutation, and the previous value is now stale (403); and (d) on `/api/overview` failure, both the Getting Started AND the Guards Active cards fall back to an error state instead of one being stuck on "Loading…".

### Added — Console e2e Phase C: session lifecycle, cross-user revoke, backup pruning, audit shape

Four new scenarios that fence the remaining operational surface of the Web Console:

- **Scenario 30 (session lifecycle)** — back-dating `user_sessions.expires_at` in SQLite makes `/api/me` return 401 on the next call (no wall-clock wait); the expired row is then deleted by the extractor (no reaper to depend on); flipping `users.disabled = 1` mid-session also returns 401 on the very next request; a fresh login for a now-disabled user is also 401.
- **Scenario 31 (cross-user revoke)** — a non-admin user trying to `DELETE /api/tokens/:id` against an id that belongs to admin returns 403 (not a quiet 200 + no-op, which would let a viewer fingerprint other users' token ids), and the row's `revoked_at` stays NULL. Viewer is also forbidden from `POST /api/users/:id/force-revoke-tokens`.
- **Scenario 32 (`backup_limit` pruning)** — sets `[console] backup_limit = 3`, performs 6 edits against a seeded dict file, and confirms `/api/backups` returns exactly 3 entries and the disk view agrees. Catches a regression where retention was off by one or pruning silently disabled.
- **Scenario 33 (audit JSON shape)** — provokes a `user_create` mutation, parses the JSONL line with `jq`, and asserts the documented envelope field by field: `actor`, `action`, `target`, `request_id` (locked to the 48-hex-char form documented at `src/audit/mod.rs:186`), `timestamp` (RFC3339-shaped), `actor_id` (stringified i64 — same column will hold OIDC subjects in Phase 4), and that `after.password_hash` is absent (password hash leak across operator logs would be a real bug).

`tools/e2e.sh` now carries 129 assertions (was 109 at PR #39 merge). Each scenario spawns a `nanoguard-console` (and a `nanoguard` proxy when the test exercises a proxy-side path) on isolated ports under a per-scenario workdir.

### Changed — `nanoguard-admin` tty prompt is now in-process

`nanoguard-admin set-password` no longer forks `test -t 0` and `stty -echo` to manage the controlling terminal. It now uses `std::io::IsTerminal` for the TTY check and `libc::tcgetattr` / `tcsetattr` (via the `libc` crate already used by the reload trigger) to mask `ECHO` off and restore on drop. CLAUDE.md absolute rule #2 — "single binary, no runtime dependencies beyond the binary itself" — applies as much to operator tools as to the proxy, so the shell-out was a quiet violation. No new crate dependency.

### Added — Console e2e: auth, permission boundary, file edit round-trip + admin CLI

`tools/e2e.sh` grows three scenarios that fence the Web Console operationally:

- **Scenario 27 (auth + permission boundary)** — wrong-password is 401 and does not echo the submitted secret; mutating endpoints with no `X-CSRF-Token` or a wrong one are 403; an admin can create a non-admin user; the non-admin can sign in and self-service-mint a proxy token; the non-admin cannot create users (403); admin force-revoke flushes the proxy verification cache so the previously-minted token immediately 401s; logout invalidates the session row, not just the cookie.
- **Scenario 28 (file edit round-trip)** — the Phase 2 file-edit machinery now has end-to-end coverage. The console writes a new `inline_block` keyword to `nanoguard.toml` through `POST /api/edit`, the edit handler fires a socket-based reload trigger, and the proxy starts blocking the new keyword without a restart. Invalid TOML is rejected by both `/api/validate` (returns `valid:false` in the body) and `/api/edit` (HTTP 400 before the atomic rename) — the pre-rename validator is what keeps a fat-fingered edit from bricking the proxy. The edit produces a backup file under `.nanoguard-backups/` (visible via `GET /api/backups`), the pre-existing keyword still blocks after the edit (no truncation regression), and `console-audit.jsonl` records the edit action.

- **Scenario 29 (nanoguard-admin CLI)** — the recovery binary itself was unverified until now. The new scenario bootstraps an admin, "forgets" the password, runs `set-password admin --password-stdin` against the offline DB, brings the console back up, and confirms the old password 401s while the new password 200s. Also fences the rejection paths: weak passwords (`is_common_password`), empty stdin, missing username, unknown user — each error message is asserted verbatim so a future refactor cannot silently turn a hard fail into a no-op.

While here, `api_force_revoke_user_tokens` (XKA-61) gains the same `trigger_invalidate_tokens` call the per-token revoke uses — without it, a mass-revoke would have left up to 60s of cached "verified" results on the proxy, which is exactly the scenario where that latency is a security bug.

### Fixed — console-issued token revoke now takes effect on the proxy immediately

The Web Console and the proxy run as separate binaries, so the proxy's in-memory client-token verification cache (default 60s TTL) used to keep a console-revoked token usable for up to a minute. A new `INVALIDATE_TOKENS\n` command on the `[reload].socket` channel asks the proxy to drop the cache only (no matcher / redactor / policy rebuild). `console::reload::trigger_invalidate_tokens` sends it from `DELETE /api/tokens/:id` and surfaces the trigger outcome in the response body so a misconfigured `[reload]` shows up at exactly the moment it bites. PID-file deployments fall back to SIGHUP, same as before. Scenario 26 in `tools/e2e.sh` stands up both binaries against a shared SQLite DB and asserts the full round-trip: console UI mints a token → proxy `/v1/chat/completions` accepts it (200) → console UI revokes it → proxy rejects it (401).

### Added — `nanoguard-admin` offline user-management CLI

New `src/bin/nanoguard-admin.rs` binary covers the lock-out path the web Console intentionally does not: a lost or forgotten admin password. Subcommands are `set-password <username>` and `list-users`. `set-password` argon2id-hashes the new value through the same `console::db::set_password_hash` write path the Web UI uses, and invalidates the affected user's live sessions so a leaked cookie cannot keep an attacker signed in. Three input modes — interactive tty prompt with echo off (default), `--password-stdin` for piping from a password manager, `--password <pw>` for scripted runs — and the plaintext lives in `Zeroizing<String>` for its entire lifetime. The binary reuses `nanoguard::console::{auth, db}` directly so the on-disk format cannot drift from the Console, and it reads `[budget].db_path` from the same `nanoguard.toml` the Console reads. Wrapper target `make set-admin-password [ADMIN_USER=alice]`; `make run-console` now points operators at this recovery path in its "already has users" message. README adds a "Recovering a forgotten admin password" section.

### Added — per-session CSRF token for console mutating endpoints (XKA-59)

Every mutating console handler now requires a per-session double-submit CSRF token: missing or mismatched header returns `403 Forbidden` before any work is done. The token is generated at session creation, persisted on the `user_sessions` row (new `csrf_token BLOB` column, migrated forward in place), surfaced to the SPA via the `/api/login` response body and on `/api/me`, and verified in constant time via `subtle::ConstantTimeEq` against the stored value. On a successful mutation the server rotates the token, persists the new value, and returns it as `X-CSRF-Token-Next` so the JS client refreshes its cached value transparently. Scope follows `docs/design/web-config-ui.md § Auth`: `POST /api/logout`, `POST /api/tokens`, `DELETE /api/tokens/:id`, `POST /api/users`, `PUT /api/users/:id`, `POST /api/edit`, `POST /api/revert`, and `POST /api/reload/trigger`. `POST /api/login` is exempt — the session that would hold the token does not yet exist. Four new extractor-level tests cover the missing / mismatched / valid / rotation paths.

- **`docs/design/release-strategy.md` (new)** — Reviews the current release process and proposes a three-phase strategy for distribution hardening and workflow automation.
- **CI/CD: Automated GitHub Releases** — `release.yml` now builds multi-arch binaries for Linux and macOS and uploads them to a GitHub Release automatically when a tag is pushed.

### Added — console-audit.jsonl for admin mutations (XKA-61)

Every mutation that flows through the console — login, logout, token create/revoke, user create/update, role change, force-revoke-all-tokens, and the existing file edit/revert — now writes a structured record to `[console].audit_path` (default `console-audit.jsonl`). Each entry carries the common envelope `{ request_id, timestamp, actor, actor_id, action, target, before, after, summary }`. `before` / `after` are JSON objects scoped to the fields the action actually changed, and are omitted entirely when an action carries neither (login, logout). The existing Phase 1 `EditRecord` shape is still accepted by the viewer so on-disk files written by older binaries continue to read cleanly.

A new admin endpoint `POST /api/users/:id/force-revoke-tokens` walks every live `client_tokens` row for the target user, marks them revoked atomically, and emits a `user_force_revoke_all` audit record with the revoked count. The viewer endpoint `GET /api/console-audit` accepts new `actor` and `target` query parameters (in addition to `action`, with `verdict` retained as a legacy alias for `action`) so operators can answer "what did admin alice do" and "what was done to user carol" without a separate jq pass. The proxy still never writes to this file; the console still never writes to the proxy audit log.

### Added — `nanoguard-console` Phase 2: file editing + reload trigger (XKA-53, XKA-58)

`nanoguard-console` can now edit the proxy's on-disk configuration through the same files the proxy reads on reload. Every write follows the contract from `docs/design/web-config-ui.md > File-write contract`: validate with the proxy's own parser, write to `.<name>.tmp.<pid>.<nonce>.<ts>` in the same directory, `fsync` the file and the parent directory, then `rename` over the target. Editable surfaces are `nanoguard.toml` (reload-safe keys), `dicts/*.txt` (keyword and PII-regex formats), and `policies/*.yaml` (rule bundles). Restart-only keys — `listen`, `log_level`, `[backends.*]`, `[budget].db_path`, `[audit].path` — remain rejected at the path-allowlist layer.

Before every rename, the prior file content is copied to `<parent>/.nanoguard-backups/<stem>.bak.<UTC-nanos>[.<ext>]`; retention is a per-file count, default 20, configurable via the new `[console] backup_limit` key (set to `0` to keep every backup). The console exposes the backups for revert through `GET /api/backups?path=…` and `POST /api/revert`, both gated by the admin role.

A successful or attempted edit emits a JSONL line to `[console] audit_path` (default `console-audit.jsonl`) with `{timestamp, actor, action, file, before_hash, after_hash, summary}` — `action` is `edit` for writes and `revert` for backup restores. This log is separate from the proxy audit log and is never written to by the proxy.

After the write, the console triggers a reload via the proxy's hot-reload surface (already shipped, commit `19fddf0`): SIGHUP through `[reload] pid_file` by default, or a `RELOAD\n` line over `[reload] socket` when set. `GET /api/reload/status?since=<epoch-seconds>` polls the proxy audit log for a `reload_ok` or `reload_failed` entry newer than the trigger, surfacing the outcome to the UI.

New REST endpoints (admin-only): `POST /api/edit`, `POST /api/validate`, `GET /api/backups`, `POST /api/revert`, `POST /api/reload/trigger`, `GET /api/reload/status`, `GET /api/console-audit`. Plus a new unit suite covering atomic writes, dict / PII / policy / TOML validators, backup retention pruning (including `backup_limit = 0` no-prune behavior), reload-status polling, and console-audit JSONL append. `docs/design/web-config-ui.md` graduates from `proposed` to `partial`; Phase 3 (remaining reloadable TOML sections) and Phase 4 (OIDC + CSRF) stay on the roadmap.

### Added — `nanoguard-console` Phase 1: read-only console + own-token self-service

Initial cut of the optional Web Configuration UI as a separate binary, `nanoguard-console`, sharing `nanoguard.toml` and the budget SQLite database with the proxy but running in its own process with its own listener. Phase 1 is read-only against the on-disk config and the budget database (opened with `SQLITE_OPEN_READ_ONLY` so a console crash can never corrupt budget state).

Authentication is local-password only: an admin is bootstrapped from `BOOTSTRAP_PASSWORD` on first start (read before the tokio runtime starts and wrapped in `Zeroizing<String>` so the plaintext is wiped from memory immediately after hashing). Sessions are cookie-based, signed with `CONSOLE_SESSION_SECRET`, default 24 h TTL, marked `Secure` automatically when the listener is non-loopback. Logged-in users see "My tokens" (create/list/revoke own proxy tokens; the wire secret is shown exactly once), "My budget", and "My audit slice". Admins additionally see user CRUD (`allowed_models`, `budget_limit`, `disabled`, `role`), an audit viewer, and a budget dashboard. The shipped binary serves a single static-asset bundle from `include_bytes!`; no Node runtime, no external auth provider. OIDC is Phase 4.

### Added — client-auth verification cache (TTL + LRU + revoke-driven invalidation)

The middleware no longer hits SQLite on every authed request. `ClientAuth::lookup` is a read-through cache keyed by token prefix: cache hit on the hot path, SQLite read + populate on miss. Bounded by `[auth].cache_capacity` (default 10_000) with `[auth].cache_ttl_secs` (default 60s) eviction. Negative results (unknown prefixes) are intentionally not cached so a flood of bogus prefixes cannot grow the cache and a newly-minted token is visible immediately.

`DELETE /v1/admin/clients/:id` resolves the row's prefix and calls `invalidate_cached` before returning, so a revoked token stops working on the very next request instead of waiting up to `cache_ttl_secs`. The TTL remains the fallback for any revocation that bypasses the admin endpoint (e.g., a direct SQLite edit).

`docs/design/client-auth.md > Caching` updates to describe what was actually built — explicit in-process invalidation rather than a tokio broadcast channel, since both producer (admin handler) and consumer (verifier) share the same `Arc<TokenCache>`. 8 new cache unit tests, 4 new runtime tests, and an updated e2e assertion at 23j (revoke now takes effect "immediately" rather than "on the next request after up to 60s").

### Fixed — `AuditLog::write` no longer drops failures silently (XKA-48)

The audit writer previously hid I/O errors (the `writeln!` return value was discarded with `let _ =`) and skipped any write that landed on a poisoned mutex (`if let Ok(mut f) = self.file.lock()`). A disk-full condition, a read-only filesystem, an NFS write failure, or a single panic in another thread holding the audit lock would all silently disable the audit trail — exactly the failure mode where a security audit log most needs to stay visible.

`write` and `write_reload` now share a `write_line` sink that recovers from `Mutex` poisoning via `poisoned.into_inner()` and emits `tracing::error!` so operators see the recovery, logs `writeln!` failures at `error` level instead of dropping them, and optionally calls `sync_all` after each write when `[audit] fsync_every_write = true` (default `false`, trading throughput for crash-durability of the most recent entries).

`new_request_id` now mixes a process-wide `AtomicU64` counter into the id (32 hex chars timestamp + 16 hex chars counter), so two ids minted in the same nanosecond no longer collide. The counter half is a collision-prevention tie-breaker, not an unpredictability guarantee — request ids are not used for authentication or capability checks. Five new audit unit tests cover the JSONL append path, the poisoned-lock recovery, the `fsync_every_write` plumbing, the reload-verdict shape, and request-id uniqueness under a tight 10k-iteration loop.

### CI — tighten lint scope, pin MSRV, add macOS matrix, deny audit warnings (XKA-49)

`.github/workflows/ci.yml` now mirrors `make preflight` instead of drifting from it: clippy runs against `--all-targets --all-features` so benches and integration tests are linted by CI, the `test` job runs as a fail-fast-disabled matrix over `ubuntu-latest` and `macos-14` so `rusqlite` (bundled) / `rustls` build breakage surfaces before release, and `cargo audit --deny warnings` so a future advisory that only emits a warning still fails CI. A new `msrv` job parses `rust-version` out of `Cargo.toml` and `cargo build --tests` on that exact toolchain — the pin now has CI enforcement instead of being a comment. `Cargo.toml` gains `rust-version = "1.82"` as the declared MSRV — the existing `src/matcher/mod.rs` uses `std::iter::repeat_n` (stabilized in 1.82), and the new `msrv` job catches exactly that drift via clippy's `incompatible_msrv` lint; `make lint` and `make preflight` are bumped to `--all-targets --all-features` and `cargo audit --deny warnings` so local and CI stay in lockstep. Windows runners are intentionally left for a follow-up — a couple of bundled deps need separate investigation under MSVC before a green matrix run is realistic.

### Fixed — `filter_output` now case-insensitive (silent PII leak on LLM responses)

`Matchers::filter_output` was running iword's `Dictionary::filter` in case-sensitive mode against raw LLM output. The output dictionary entries are lowercase (`ssn`, `social security`, `credit card`), but LLMs almost always emit these capitalized (`SSN`, `Social Security`, `Credit Card`), so the last-line-of-defence PII mask was silently bypassed for the common shape. The filter now scans a lowercased copy of the response and projects matched byte ranges back onto the original text, preserving user-visible casing / whitespace / NFKC form everywhere except the masked spans. Vault placeholders such as `[SSN_1]` are explicitly skipped so the existing PII round-trip restoration still works. Five new matcher tests cover the uppercase and capitalized variants plus the placeholder skip.

### Added — client-token authentication (Stage 1, opt-in)

Bearer client-token verification on the proxy endpoints, gated behind `[auth] enabled = false` by default so existing deployments are unaffected. When enabled, requests to `/v1/chat/completions`, `/v1/messages`, and `/v1/models` must carry `Authorization: Bearer ng_<env>_<24 char secret>` or get a 401. `/health` stays unauthenticated for load-balancer probes; `/v1/admin/*` keeps its existing `ADMIN_API_KEY` gate.

Tokens are stored as `(prefix, sha256(wire))` in the `client_tokens` SQLite table (same file as `[budget].db_path`). The full wire form is shown exactly once on creation and never persisted. Verification is constant-time (`subtle::ConstantTimeEq`) to defeat timing side-channels.

New admin endpoints (gated by the same `ADMIN_API_KEY` Bearer):

- `POST /v1/admin/clients` mints a token, returning the wire form once.
- `GET /v1/admin/clients?user_id=N` lists by prefix only.
- `DELETE /v1/admin/clients/:id` revokes (idempotent).

10 new e2e assertions in scenario 23 cover the full mint → use → list → revoke → re-reject cycle. `docs/design/client-auth.md` moves from `proposed` to `partial` (Stage 1 done; in-memory cache + per-token PII overrides + user-management integration remain).

### Docs — provider matrix expansion entered the roadmap

`docs/roadmap.md` gains a "Provider matrix expansion (post-v1, opt-in)" entry covering when and how nanoguard might grow its adapter set beyond the current OpenAI / Anthropic / Ollama trio. The entry codifies four design lines that any future Gemini / Bedrock / Cohere / Vertex adapter must hold: pluggable behind Cargo features so the default binary stays small, OpenAI-shaped IR so the translation layer stays at N+N rather than N², guardrails stay first-class (every adapter must be audit-symmetric with the OpenAI path), and the README continues to recommend LiteLLM downstream as the default deployment for 100+-provider needs. `docs/design/multi-backend-routing.md > Open questions` gains a back-reference so the design-doc reader sees the same bounds.

### CI — opt-in Aikido SCA scan

New `.github/workflows/aikido.yml` runs Aikido's dependency vulnerability scan on push to `main` and on PRs. The job is gated by the repo variable `AIKIDO_ENABLED == 'true'` so it stays inert until an operator opts in (the repo also needs the `AIKIDO_SECRET_KEY` secret provisioned from the Aikido dashboard), and skips PRs from forks (which don't have access to the secret). Third-party actions are pinned to immutable commit SHAs with the matching tag in a comment — relevant supply-chain hygiene for a security scanner. `continue-on-error: true` means a real finding shows up as an advisory check, not a merge-blocker, until we have a release cycle of clean runs and promote it to required. Setup instructions live in `docs/operations.md > External CI scans`; the tool-decision write-up lives in `docs/research/aikido-sca.md`. The scan complements (does not duplicate) the existing CodeRabbit / Greptile / Qodo review bots — those reason about diffs, Aikido scans the dependency tree against CVE feeds.

### Docs — README catches up with v0.4 → present feature set

README now advertises the features that landed across v0.4 / v0.5 / v0.6 / v0.7 / the in-flight hot-reload work: Tool gate (allow / deny / schema / entity scan), Output JSON Schema validation, Policy bundles (YAML rule sets with stable ids / severity / compliance tags), and Hot reload (SIGHUP-driven atomic config swap). The proxy architecture diagram at the top reflects the actual pipeline shape today instead of the v0.3 sketch. Cross-links to `docs/design/` and `docs/operations.md` for the deeper material.

### Docs — operations runbook + audit log format

Two new public docs that pair with the hot-reload feature: `docs/operations.md` is the operator-facing runbook (systemd unit, SIGHUP usage, logrotate snippet, common-issues section), and `docs/design/audit-log-format.md` is the JSONL schema reference for both request entries and the new reload entries. The audit-log doc enumerates the bounded `error` label vocabulary used in `reload_failed` entries so audit-pipeline consumers can match against a stable set.

### Added — hot reload via SIGHUP

The proxy now picks up config / dict / policy bundle changes without a restart. SIGHUP triggers a rebuild of the matcher, redactor, spotlight, schema, tool gate, and policy index; on success the new state is atomically swapped in (lock-free via `arc-swap`). In-flight requests finish on the snapshot they acquired at handler entry, so a reload mid-request never produces a half-applied filter pass.

Reload is all-or-nothing: a TOML parse error, an invalid policy YAML, a regex that fails to compile, or any other build failure leaves the live state untouched and writes a `reload_failed` audit entry. A successful reload writes `reload_ok`. Restart-only knobs (listen address, log level, backend pool, budget DB, audit file handle) are isolated in a `RuntimeHandles` struct that survives across reloads.

`docs/design/hot-reload.md` graduates from `proposed` to `shipped`. Two new e2e scenarios (20: SIGHUP picks up a new block rule; 21: invalid config keeps live state) bring the e2e count from 41 to 47 assertions.

### Docs — multi-user auth & routing design set

Four new / revised design docs that together describe how nanoguard becomes a real policy boundary for multi-user deployments. All status `proposed`; no implementation yet.

- **`docs/design/client-auth.md` (new)** — Bearer client token model for the proxy endpoints. Tokens are `ng_<env>_<rand24>` with prefix+hash storage, verified on the hot path through an in-memory cache with explicit invalidation. The audit log's `api_key` field becomes the verified token prefix rather than a self-asserted body field. A three-stage rollout (off → shadow → enforced) avoids breaking existing deployments.
- **`docs/design/multi-backend-routing.md` (new)** — replaces single `[backend]` with `[backends.*]` + `[routing]` so one proxy can fan out to OpenAI, Anthropic, Ollama, DeepSeek, etc. Per-user `allowed_models` enforces the admin-controlled "which user can hit which model" matrix. Migration from `[backend]` is backwards-compatible for one release.
- **`docs/design/user-management.md` (new)** — user data model, OIDC-first / local-password-fallback console login, self-service token issuance lifecycle (create / list / revoke / relabel), per-user admin actions (allowed_models, budget_limit, role, force-revoke). Sessions are cookie-based and separate from proxy Bearer tokens.
- **`docs/design/web-config-ui.md` (revised)** — incorporates the three above: user self-service token UI, admin per-user policy editor, model-allowlist editor backed by `[routing]`. Phase plan re-ordered to land own-token self-service first (Phase 1), then file editing (Phase 2), then OIDC (Phase 4). Static-token mode dropped in favor of local password + OIDC.

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
