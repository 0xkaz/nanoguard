> **Status:** proposed (2026-05-16)

# Web Configuration UI

nanoguard today is configured by editing `nanoguard.toml`, the dict
files under `dicts/`, and the policy bundle under `policies/`,
followed by a process restart. The forthcoming hot-reload work
(`docs/design/hot-reload.md`) removes the restart step. This document
proposes a small, optional Web UI for editing those same files and
for browsing audit / budget state.

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
per-form CSRF token. Every successful submit follows the same
contract:

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
- `[backend] *`
- `[budget] db_path`, `[audit] path`

## Auth

The console is authenticated. There is no anonymous mode.

```toml
# nanoguard-console.toml
[console]
listen      = "127.0.0.1:8081"   # default: loopback only
audit_path  = "nanoguard-audit.jsonl"
budget_db   = "nanoguard.db"
config_root = "."                # dir holding nanoguard.toml, dicts/, policies/
proxy_pid_file = "nanoguard.pid"

[console.auth]
mode = "static_token"            # "static_token" | "oidc"
token = "${CONSOLE_TOKEN}"       # env-substituted

[console.auth.oidc]              # only when mode = "oidc"
issuer       = "https://idp.example.com"
audience     = "nanoguard-console"
allowed_subs = ["alice@example.com"]
```

- **Default mode** is `static_token`. The console rejects any
  request without `Authorization: Bearer <token>` matching the
  configured value. The token comes from an env var, never the
  config file directly.
- **Listen address defaults to `127.0.0.1`** — loopback only. To
  expose the console on a network interface, the operator changes
  `listen` explicitly. This is opt-in, not the default, because a
  misconfigured console on a public IP is the canonical "remote
  config" disaster.
- **OIDC mode** is the alternative for organizational deployments.
  The console verifies an ID token against the configured issuer
  and accepts only the `sub` values in `allowed_subs`. No
  per-user database; the IDP is the source of truth.
- **CSRF**: every mutating form embeds a per-session double-submit
  token. The token rotates on every successful mutation.
- **Audit context**: every edit records the authenticated subject
  (`actor`) in the console's own audit log. For static-token mode,
  `actor = "static_token"`; for OIDC, `actor = <sub>`.

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

- **The console is privileged.** Anyone who can authenticate to it
  can edit guardrail rules. Treat the console token like a root
  password — store it in a secret manager, rotate, scope by host.
- **Default loopback** + token auth is appropriate for a single
  operator on a known host. **Network-exposed** consoles must
  terminate TLS in front of the console (reverse proxy or
  nanoguard-console itself with rustls + a real cert) and SHOULD
  use OIDC, not static token.
- **The console cannot delete audit log entries.** The audit log
  file is opened read-only by the console. Edit records the
  console writes are appended to a separate `console-audit.jsonl`,
  not the proxy's audit log.
- **The console cannot write to the budget DB.** Read-only
  connection only. Changing limits goes through the existing
  `/v1/admin/budget/*` API on the proxy (where the existing Bearer
  auth applies); the console UI calls that API on the user's
  behalf, with the operator's token, never with the proxy's admin
  token directly.

## Implementation outline

This is a phase plan, not a commitment. Each phase ships
independently.

1. **Phase 1 — read-only console.** Audit viewer + budget
   dashboard + current-configuration view. No mutation. No
   dependency on hot reload. Static-token auth, loopback default.
   Ships as `nanoguard-console` binary in the same workspace, with
   a vendored static asset bundle (HTML/CSS/JS, no Node toolchain
   at runtime).
2. **Phase 2 — file editing for dicts and policies.** Depends on
   hot reload landing. Adds the file-write contract, the backup
   directory, the SIGHUP trigger, and the reload-outcome polling.
3. **Phase 3 — TOML section editor.** Adds structured editing of
   `nanoguard.toml` for reloadable keys. Restart-only keys remain
   read-only in the UI.
4. **Phase 4 — OIDC + CSRF hardening.** Replaces static-token mode
   for organizations that need it. Adds the per-session
   double-submit token. Documents how to put the console behind a
   reverse proxy.
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
