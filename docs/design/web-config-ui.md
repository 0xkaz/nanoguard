> **Status:** proposed (2026-05-16, revised 2026-05-16 to incorporate user-management and multi-backend routing)

# Web Configuration UI

nanoguard today is configured by editing `nanoguard.toml`, the dict
files under `dicts/`, and the policy bundle under `policies/`,
followed by a process restart. The forthcoming hot-reload work
(`docs/design/hot-reload.md`) removes the restart step. This document
proposes a small, optional Web UI for editing those same files and
for browsing audit / budget state.

It is also the surface where **users self-serve their own proxy
tokens** and where **admins manage per-user policy** — the user-
facing side of `docs/design/client-auth.md`,
`docs/design/user-management.md`, and
`docs/design/multi-backend-routing.md`. Those three docs define the
data model and on-the-wire behavior; this doc defines what the
human sees.

The UI is **not part of the core proxy**. It ships as a separate
binary, `nanoguard-console`, that links the same crate as the
library but runs its own process with its own listener, auth, and
audit boundary. The proxy hot path never gains an HTTP surface for
mutation.

## Why this is not in the core proxy

The proxy invariants from `CLAUDE.md` rule out putting mutation
endpoints on the proxy itself:

1. **Single binary, audited surface.** The proxy is what runs in
   front of every LLM request. Adding a settings-editor handler
   inside it widens the attack surface (XSS, CSRF, authz bypass,
   form-parser dependencies) and the dependency tree (HTML
   templating, static asset serving). What you audit must be what
   runs.
2. **Transparent proxy.** The proxy never modifies request
   semantics. A mutation endpoint that rewrites the rule set is a
   different concern from filtering requests; sharing a process is
   asking for layering bugs.
3. **Configuration is a file in version control.** The audit story
   for "what was the policy at 14:32 on Tuesday" is `git log
   policies/`. A web form that writes directly to in-memory state
   destroys that story. Writing to files keeps the story intact.

The Web UI therefore sits **next to** the proxy, not inside it. It
writes to the same files the proxy reads on reload, and triggers a
reload through a signal or a reload IPC.

## Goals

1. **Browse audit log and budget state** through a browser. This is
   the most-requested operational surface and the lowest-risk piece
   to ship — it is read-only.
2. **Edit `nanoguard.toml`, `dicts/*.txt`, and `policies/*.yaml`**
   through forms that validate the input, write to the file, and
   then trigger a hot reload. After a successful reload, the change
   is live.
3. **Make every edit reviewable.** Every form submit produces a
   structured diff against the previous on-disk content and an
   audit entry. Operators can post-hoc inspect "who changed what,
   and when."
4. **Stay self-hostable, offline, single-binary.** Same constraints
   as the proxy: no cloud, no Python, no external auth provider
   required (though OIDC pluggability is open).
5. **Let users self-serve their own proxy tokens.** A user logs in
   to the console, creates and revokes their own Bearer tokens,
   and views their own budget and audit slice. Admins do not
   hand-deliver credentials.
6. **Let admins manage per-user policy.** Allowed models, budget
   limits, enable/disable, force-revoke — all through the UI, not
   through editing TOML by hand. The TOML-edit path remains for
   bootstrap and recovery.

## Non-goals

- **In-memory rule editing without writing to disk.** All edits go
  to files first. The proxy reloads from files. Skipping the file
  step would diverge runtime state from git.
- **Multi-tenant administration.** A single console instance
  manages a single proxy instance. Fleet management is out of
  scope.
- **WYSIWYG policy authoring.** YAML and TOML are the source
  format; the UI shows them in a structured editor with validation,
  but does not pretend the underlying file is anything else.
- **Replacing `git`.** The console writes to working-tree files; it
  does not commit, push, or run CI. Operators who want a
  reviewable workflow point the console at a checkout and commit
  manually (a future change-request mode is open).

## Architecture

```
                ┌────────────────────────────────┐
  browser ───▶  │     nanoguard-console (UI)      │
                │  - serves HTML/JS               │
                │  - read-only:                   │
                │      audit JSONL, budget DB     │
                │  - mutating:                    │
                │      writes files, then         │
                │      sends reload IPC           │
                └──────────┬─────────────────────┘
                           │
              same machine │  (file path + SIGHUP)
                           ▼
                ┌────────────────────────────────┐
                │       nanoguard (proxy)         │
                │  - reads files on reload        │
                │  - serves /v1/chat/completions  │
                │  - never exposes mutation       │
                └────────────────────────────────┘
```

`nanoguard-console` and `nanoguard` are independent processes. They
share three things: the on-disk configuration files, the audit log
file path, and the budget DB path. The console is read-only against
the budget DB and the audit log; it is write-only against the
configuration files. It does not connect to the proxy over HTTP.

`/v1/admin/budget/*` on the proxy stays as-is — it is the
machine-to-machine API. The console is the human-to-files API.

## Surface

### Read-only views

1. **Audit log viewer.** Tail the JSONL file, filter by verdict
   (`block` / `alert` / `flag` / `reload_*`), by rule id, by
   matched category. Show the last N entries; let the user
   download a filtered slice. The console reads the file from disk;
   it does not subscribe to a stream on the proxy.
2. **Budget dashboard.** Per-API-key usage / limit, sorted by
   utilization. The console opens the SQLite DB in read-only mode
   (`mode=ro`) so a console crash cannot corrupt budget state.
3. **Current configuration view.** Renders `nanoguard.toml`,
   `dicts/*.txt`, and `policies/*.yaml` as the user would see in an
   editor, plus a "loaded version" indicator pulled from the
   proxy's most-recent `reload_ok` audit entry. If the on-disk
   files are newer than the last `reload_ok`, the UI flags
   "pending reload."

### Mutating views

Mutating views are gated by an auth check (see Auth) and a
per-session double-submit CSRF token (see Auth § CSRF). Every
successful submit follows the same contract:

```
1. UI validates the form client-side (cheap rejection of
   malformed YAML, regex compile errors, etc.).
2. UI POSTs the change to the console.
3. Console re-validates server-side (defense in depth).
4. Console writes to a temp file in the same directory, fsyncs,
   then renames over the target file (atomic replace).
5. Console writes an "edit" record to its own audit log:
      { actor, timestamp, file, before_hash, after_hash, summary }
6. Console sends a reload trigger to the proxy (SIGHUP by default,
   or a Unix-socket "reload" message — see Reload trigger below).
7. Console polls the proxy's audit log tail until it sees a
   reload_ok or reload_failed entry whose timestamp is later than
   the trigger.
8. UI shows the outcome:
      - reload_ok   → "Change applied at HH:MM:SS."
      - reload_failed → show the proxy's error message and offer to
                        revert (which writes the previous file back
                        and triggers another reload).
```

Editable surfaces:

- `nanoguard.toml` — section-by-section forms, with the same
  validation the proxy runs at startup.
- `dicts/*.txt` — table-based editor (word, key, weight). Bulk
  upload accepts a pasted block in the documented format.
- `policies/*.yaml` — rule-by-rule editor. Pattern field shows a
  live regex compile error if the rule has `regex:` semantics.

Non-editable surfaces (require restart, surfaced as read-only in
the UI with an explanatory note):

- `[nanoguard] listen`, `[nanoguard] log_level`
- `[backends.*]` endpoint, api_key, and provider (see
  `multi-backend-routing.md`; entries added/removed require restart;
  `[routing]` and per-user allowed_models are hot-reloadable)
- `[budget] db_path`, `[audit] path`

### User self-service views

After a logged-in user authenticates (`docs/design/user-management.md`),
they see a "My account" surface:

1. **My tokens.** A table of the user's tokens: prefix (e.g.
   `ng_p_a3k7…`), label, created_at, last_used_at, expires_at,
   revoked_at. A "Create token" button opens a modal asking for
   label and optional expiry. On submit, the full secret is shown
   exactly once with a copy-to-clipboard button; the modal explains
   that the secret cannot be recovered after closing. The row in
   the table immediately reflects the new token (prefix-only).
   Revoke is one click + confirmation; relabel is inline.
2. **My budget.** Personal usage / limit, per-token breakdown,
   per-model breakdown over the last 7 / 30 days. Read-only.
3. **My audit slice.** The audit JSONL filtered server-side to
   `user_id = $self`. Same filters as the admin audit viewer
   (verdict, rule id, category), narrower data.
4. **My profile.** Display name and email (read-only for OIDC,
   editable for local). Change-password form for local users. List
   of active sessions with "revoke" buttons.

These views are available to every authenticated user, regardless
of role. They never expose anyone else's data.

### Admin views

In addition to the editing surfaces above and the read-only
audit/budget dashboards, admin users see:

1. **Users.** A table of all users with filters (role, disabled,
   service_account). Row actions: edit, force-revoke-all-tokens,
   disable/enable, delete. The "edit user" panel exposes the
   per-user policy form:
   - **Allowed models.** A multi-select populated from the current
     `[routing]` table. The UI prevents selecting a model that has
     no route — the misconfig case from
     `multi-backend-routing.md` step 5 can't happen through the UI.
     Wildcard entries (`*`, `claude-*`) are typed in a free-text
     field alongside the picker.
   - **Budget limit.** Integer tokens, with a "no limit" toggle.
   - **Role.** `user` / `admin`. Changing to `admin` requires the
     current admin to type the target username as a confirmation —
     a small friction to prevent accidental privilege escalation.
   - **Disabled.** A toggle; disabling broadcasts an immediate
     cache invalidation to the proxy.
   - (Future) **Per-user PII overrides** and per-user spotlight
     overrides: reserved space; not in initial scope.
2. **Create user.** Local mode: username + temporary password.
   OIDC mode: manual `oidc_sub` entry (rare; usually auto-
   provisioned at first login). The form sets defaults from
   `[console.auth].default_role` and `default_allowed_models`.
3. **Model allowlist editor.** A view of the current `[routing]`
   table with an "Add model" form. Each row picks a backend from
   the configured `[backends.*]`. Saving updates `[routing]` in
   `nanoguard.toml` and triggers reload. The form refuses to save
   a model whose only matching backend has no `api_key` configured
   (so the operator catches the misconfig at edit time, not at
   request time).
4. **Backends overview.** Read-only summary of `[backends.*]`:
   provider, endpoint, whether `api_key` is set. Editing a
   backend entry (or adding/removing one) currently requires a
   restart; the UI explains this and offers a downloadable
   updated `nanoguard.toml` instead of an in-place edit.
5. **Console-audit viewer.** The `console-audit.jsonl` log:
   "admin alice changed allowed_models for carol from [X] to
   [X, Y] at 14:32." Filterable by actor and by target. This is
   separate from the proxy's audit log; it documents administrative
   actions, not LLM traffic.

## Auth

The console is authenticated. There is no anonymous mode.

The authentication model is defined in
`docs/design/user-management.md`: OIDC is the primary path, local
password is the fallback for deployments without an IdP, and both
can coexist. Sessions are cookie-based and short-lived; the
console never accepts the proxy's Bearer tokens for its own
endpoints. (Those tokens are for the proxy; the console issues
them but does not consume them.)

A condensed view of the relevant config — see `user-management.md`
for the full schema:

```toml
[console]
listen         = "127.0.0.1:8081"     # default: loopback only
session_secret = "${CONSOLE_SESSION_SECRET}"

[console.auth]
mode = "oidc"                          # "oidc" | "local" | "both"

[console.auth.oidc]
issuer       = "https://idp.example.com"
client_id    = "nanoguard-console"
client_secret = "${OIDC_CLIENT_SECRET}"
redirect_uri = "https://console.example.com/oauth/callback"
admin_claim  = { name = "groups", value = "nanoguard-admins" }
auto_provision = true

[console.auth.local]
allow_signup = false
bootstrap_admin = { username = "admin", password_env = "BOOTSTRAP_PASSWORD" }
```

- **Listen address defaults to `127.0.0.1`** — loopback only. To
  expose the console on a network interface, the operator changes
  `listen` explicitly. This is opt-in, not the default, because a
  misconfigured console on a public IP is the canonical "remote
  config" disaster.
- **CSRF**: every mutating endpoint requires a per-session
  double-submit token (`X-CSRF-Token` request header, hex-encoded
  32 bytes). The token is generated at session creation, stored on
  the `user_sessions` row, returned in the `/api/login` response
  body and on `/api/me`, and verified in constant time before any
  side-effecting work. On a successful mutation the server rotates
  the token, persists the new value to the session row, and returns
  it via the `X-CSRF-Token-Next` response header so the JS client
  refreshes its cached value. `POST /api/login` is the only
  mutating endpoint exempt from the check, since the session
  required to hold a token does not yet exist.
- **Audit context**: every edit records the authenticated subject
  (`actor`) in `console-audit.jsonl`. The `actor` is the username
  (local mode) or the OIDC `sub`.

## Reload trigger

How the console tells the proxy to reload is intentionally
narrow:

1. **SIGHUP via PID file** (default). The console reads
   `proxy_pid_file`, checks the PID exists, then sends SIGHUP. The
   PID file is written by the proxy at startup (a new feature
   landed with hot reload).
2. **Unix domain socket** (optional). If `[reload] socket = "..."`
   is set in the proxy config, the proxy listens on that socket
   for a single line: `RELOAD\n`. The console writes that line and
   reads the immediate `OK\n` or `ERR <message>\n`. This avoids
   PID file races on systems that wrap nanoguard in a process
   supervisor.

There is **no TCP reload endpoint**. The console talks to the
proxy through Unix primitives only, on the same host. Cross-host
deployment uses one console per host.

## File-write contract

Every write follows three invariants:

1. **Atomic.** Write to `path.toml.tmp.<pid>.<nonce>` in the same
   directory, fsync, then `rename` over the target. A power loss
   during the write leaves either the old or the new file, never a
   torn one.
2. **Validated.** The console runs the same parser the proxy will
   run at reload time before doing the rename. A YAML or TOML
   parse failure aborts the write before any file changes.
3. **Backed up.** Before every rename, the previous content is
   copied to `path.bak.<timestamp>` in a `.nanoguard-backups/`
   directory (created on first write). Retention is N backups
   per file, configurable, default 20. The "revert" button reads
   from this directory.

The backup directory is intentionally in the repo working tree so
operators can `git diff` the backup against the live file.

## Interaction with hot reload

The console depends on hot reload, but it does not replace it.

- Hot reload (proxy-side) defines the **atomic-swap unit** and the
  **failure semantics**: an invalid config never replaces the live
  one; a failed reload writes `reload_failed`. The console
  consumes those semantics and surfaces them in the UI.
- The console adds **edit semantics**: who made the change, what
  the diff was, when it was triggered. The proxy itself does not
  know there was an "edit"; from its perspective, files on disk
  changed and a SIGHUP arrived.
- A reload triggered by SIGHUP from a shell (no console) still
  works exactly as before. The console does not gatekeep reloads.

## Threat model and limits

- **Admin sessions are privileged.** An authenticated admin can
  grant themselves any permission, edit policies, and revoke any
  token. The mitigations are named admins (no shared accounts),
  MFA at the IdP layer for OIDC mode, the `console-audit.jsonl`
  for after-the-fact accountability, and step-up confirmation on
  role escalation.
- **Default loopback** + named user auth is appropriate for a
  single operator or small team on a known host. **Network-
  exposed** consoles must terminate TLS in front of the console
  (reverse proxy or `nanoguard-console` itself with rustls + a
  real cert) and SHOULD use OIDC, not local password.
- **The console issues proxy tokens but does not consume them.**
  Tokens are for the proxy. A console session cookie is a
  separate credential and is HttpOnly / Secure / SameSite=Lax.
- **The console cannot delete proxy audit log entries.** The
  proxy's audit log file is opened read-only by the console.
  Admin actions are appended to the separate
  `console-audit.jsonl`, which the proxy does not write to.
- **Budget DB writes go through the proxy's admin API.** The
  console opens the SQLite file read-only for dashboards;
  changing limits posts to `/v1/admin/budget/*` on the proxy
  using the admin's session-bound short-lived token. The console
  never holds the proxy's `ADMIN_API_KEY` directly in a way that
  is exposed to admins through the UI.

## Implementation outline

This is a phase plan, not a commitment. Each phase ships
independently. The order is shaped by the dependency graph: the
console depends on `client-auth.md` (for per-user budget views,
for issuing tokens) and `user-management.md` (for the user data
model). File editing depends on hot reload.

1. **Phase 1 — read-only console + local-password auth + own-token
   self-service.** Depends on `client-auth.md` + Phase 1/2 of
   `user-management.md`. Ships:
   - Local-password login (bootstrap admin from env)
   - "My tokens" / "My budget" / "My audit slice" for every user
   - Read-only audit viewer, budget dashboard, current-config view
     for admins
   - No file editing yet, no OIDC yet
   `nanoguard-console` binary in the same workspace, vendored
   static asset bundle (HTML/CSS/JS, no Node toolchain at runtime).
2. **Phase 2 — file editing for dicts, policies, and the
   per-user policy editor.** Depends on hot reload landing. Adds:
   - File-write contract (atomic rename + backup) for dicts /
     policies / TOML keys that are reload-safe
   - Admin user editor (allowed_models, budget_limit, disabled,
     role)
   - Model allowlist editor backed by `[routing]`
   - Force-revoke-all-tokens, console-audit log surface
3. **Phase 3 — full TOML section editor.** Adds structured editing
   of remaining reloadable `nanoguard.toml` keys (input,
   spotlight, schema, tool gate). Restart-only keys (listen,
   backends.*, audit.path, budget.db_path) remain read-only in the
   UI.
4. **Phase 4 — OIDC + CSRF hardening.** Adds the OIDC login flow
   from `user-management.md`, with auto-provisioning and
   admin-claim mapping. Per-session double-submit CSRF token.
   Documents how to put the console behind a reverse proxy.
   Static-token auth (the simpler single-operator mode that was
   the earlier version of this doc) is dropped: local password and
   OIDC are the two supported modes.
5. **Phase 5 (optional) — change-request mode.** Instead of
   writing the file directly, the console writes to a worktree
   branch and opens a PR via the local `gh` CLI. The reload only
   happens after the PR merges into the working tree (out-of-band
   CI). This is the high-assurance flow and is purely additive.

## Open questions

- **Static-asset bundle**: include via `include_bytes!` in the
  console binary, or serve from a `console-assets/` directory next
  to the binary? Binary inclusion is simpler for the single-binary
  story; a directory is easier to patch in production. Direction:
  bundle in the binary, accept a `--assets-dir` override.
- **Backup retention policy**: a flat per-file count (the default
  here) is the simplest. Time-based (`older than 30 days`) is
  another option. Open until phase 2.
- **Multi-process safety**: two console instances writing
  concurrently to the same file. The atomic rename keeps the file
  consistent, but two edits can lose work. A file-lock on the
  config_root during a write is the likely answer; open until we
  see whether anyone runs more than one console per host.
- **API for external automation**: every console action could
  also be a JSON API call, which would make CI-driven edits
  possible. Keeping the door open by designing the routes as
  REST-shaped from phase 1, but explicitly *not* documenting them
  as stable until phase 4.
