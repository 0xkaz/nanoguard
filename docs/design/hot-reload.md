> **Status:** shipped (commit 19fddf0, 2026-05-16)

# Hot Reload

nanoguard's entire request-side configuration — keyword rules,
PII patterns, spotlight transform, JSON Schema validators, Tool Gate
allow/deny, the policy YAML bundle — is built once at startup and
stored on `AppState` behind `Arc<_>`. Today, changing any of those
requires a restart. Hot reload makes that swap atomic and in-process,
without touching the listening socket, the budget database, or the
audit log file.

The single-binary, offline-first, transparent-proxy invariants from
`CLAUDE.md` are not negotiable. Hot reload must not weaken any of
them.

## Goals

1. **Reload rules without dropping connections.** SIGHUP (and an
   optional file-watch trigger) rebuilds the matcher / redactor /
   guards / policy index and atomically swaps them. In-flight
   requests finish on the previous configuration; the next request
   sees the new one.
2. **All-or-nothing.** A reload that fails validation does not
   partially apply. The proxy keeps serving with the previous
   configuration and records a `reload_failed` audit entry.
3. **No restart for policy churn.** This is the deployment story
   for edge devices, robotics, and air-gapped environments — pushing
   a new `policies/*.yaml` over rsync and signalling the process
   should be the whole operation.

## Non-goals

- **Reloading the listening address.** A change to `[nanoguard]
  listen` requires a restart. Same for `log_level` (tracing
  subscriber is installed once).
- **Reloading the budget store.** SQLite handle, WAL state, and
  spend records are runtime state, not configuration.
- **Reloading the audit log file handle.** The file is open in
  append mode and must not be rotated by reload (use logrotate +
  `copytruncate` or a future `SIGUSR1` for rotation).
- **Reloading the backend `reqwest::Client`.** Connection pool state
  is runtime, not config-derived. Backend endpoint and API key
  changes require a restart.
- **Per-request live reload.** There is no "reload between phases of
  a single request." A request that has begun keeps its snapshot.

## Swap unit

The smallest atomic unit is the entire bundle of config-derived
state, not individual modules. Reloading the matcher without the
redactor — or vice versa — produces a window where the policy bundle
has been re-parsed but only some derived state has caught up.

Concretely, an immutable `ReloadableState` carries every field on
`AppState` that is built from `nanoguard.toml`, the dict files, and
the policy bundle:

```rust
struct ReloadableState {
    config:       Arc<Config>,                // config-derived view (not listen/log)
    matchers:     Arc<Matchers>,
    redactor:     Arc<Redactor>,
    pii_actions:  Arc<ActionPartition>,
    spotlight:    Option<Arc<SpotlightConfig>>,
    schema:       Option<Arc<SchemaValidator>>,
    tool_gate:    Option<Arc<ToolGate>>,
    policy:       Option<Arc<PolicyRuleIndex>>,
}
```

`AppState` then holds `ArcSwap<ReloadableState>` plus the
non-reloadable fields (`http_client`, `budget`, `audit`, server
config). Request handlers call `state.reloadable.load()` once at
the start of the request and use that snapshot for the duration of
the request — including the SSE response path, the Vault round-trip,
and the streaming Tool Gate accumulation.

`ArcSwap` is chosen for lock-free load on the hot path: the snapshot
acquisition becomes a single atomic pointer load with no read-write
lock contention. The alternative — `RwLock<Arc<ReloadableState>>` —
would put a read lock on every request and serialize against
reloads.

## Trigger surface

Two trigger mechanisms, both optional, both enabled by config:

```toml
[reload]
enabled    = true     # default false; opt-in for v0.8.0
on_sighup  = true     # default true when [reload].enabled
watch      = false    # default false; opt-in fs watch
watch_paths = [
  "nanoguard.toml",
  "policies/",
  "dicts/",
]
debounce_ms = 250
```

- **SIGHUP** is the primary trigger. Unix-only, well-understood,
  trivial to script. The signal handler enqueues a reload request to
  a single background task; coalescing multiple SIGHUPs that arrive
  while a reload is in progress is required.
- **File-system watch** is opt-in and uses `notify` (the
  cross-platform Rust crate). Debounced because editors typically
  emit several events per save. Useful in development; defaulted off
  in production because it widens the surface.

There is **no HTTP reload endpoint**. Adding one would require auth,
CSRF protection, and rate limiting on the proxy hot path, and it
contradicts the "configuration is a file in the filesystem" story.
Operators who want a remote trigger should send a SIGHUP via SSH or
a sidecar.

## Reload procedure

A reload runs on a single dedicated tokio task. Multiple triggers
that arrive while one reload is running collapse to a single
follow-up reload (a tail-coalescing pattern, not a queue).

```
1. Read nanoguard.toml from disk.
2. Load the policy bundle named in [policies] bundle_path.
3. Build a candidate ReloadableState:
      - matcher (aho-corasick or iword) compiled from keyword config
        + policy literal rules
      - redactor compiled from default + dict + policy regex patterns
      - spotlight, schema, tool_gate, policy_index per their config
4. If any step in (1)-(3) returns an error:
      - log the error at WARN
      - write an audit entry: { verdict: "reload_failed", error: "..." }
      - leave the live ReloadableState untouched
      - reload task returns; proxy continues serving
5. ArcSwap::store(new_state) — atomic pointer swap.
6. Write an audit entry: { verdict: "reload_ok",
                           rules_loaded: N,
                           policy_version: "..." }
```

The audit log records reload outcomes so a JSONL tail or central
audit pipeline can detect a silent rollback. A failed reload **must
not** be silent — it is the difference between "this rule was
deployed" and "this rule was deployed but the live process is still
running the old set."

## Per-request snapshot contract

A request handler MUST call `state.reloadable.load()` exactly once,
at the top of the handler, and reuse that `Arc` for every subsequent
guard decision in the same request — including the streaming
response path. Re-loading mid-request would produce a request whose
input was scanned by configuration A but whose output is filtered by
configuration B. That breaks the proxy's auditability: the audit
entry records one matched rule set, but two were actually consulted.

This contract is enforced by handler shape, not by the type system.
Reviewers should flag any guard call site that loads the snapshot
twice in a single request.

## What is not reloadable, and why

| Field                  | Why restart-only                                              |
|------------------------|---------------------------------------------------------------|
| `nanoguard.listen`     | TcpListener is bound once; can't be moved.                    |
| `nanoguard.log_level`  | `tracing_subscriber::set_global_default` is one-shot.         |
| `backend.*`            | `reqwest::Client` owns a connection pool; swap would orphan keep-alives. |
| `budget.*`             | SQLite handle + WAL state; reopening mid-operation risks lost spend. |
| `audit.*`              | Open file handle; rotation needs a separate signal (future). |

Changes to any of the above require a graceful restart. The config
parser will continue to accept changes to those keys on reload —
they simply produce a `reload_partial` audit entry noting which
keys were ignored.

## Interaction with existing features

- **Vault round-trip** is per-request and per-snapshot. A reload
  between request and response does not affect a request already in
  flight, because the response-side deanonymizer holds the
  `Arc<Redactor>` it was created with.
- **SSE streaming** uses the request-time snapshot for the entire
  stream lifetime. A reload during a 60-second streaming response
  does not switch filters mid-stream.
- **Shadow mode** transitions cleanly: a request starting after the
  swap sees `shadow = true/false` as configured in the new state.
- **Policy bundle audit metadata** (`rule_id`, `category`,
  `severity`, `compliance`) is rebuilt with the matcher, so audit
  entries written after the swap reference the new bundle's rule
  ids. Audit entries written before the swap retain the old rule
  ids — this is correct behavior, not drift.

## Failure modes

1. **TOML parse error in `nanoguard.toml`** → reload aborts, audit
   `reload_failed`, live state retained.
2. **Policy YAML bundle invalid** → same.
3. **Dict file missing or unreadable** → same.
4. **Regex in a dict or policy rule fails to compile** → same.
5. **JSON Schema in `[output.schema]` or tool definition invalid**
   → same.
6. **OOM while building a large aho-corasick automaton** → reload
   task panics; the panic is caught at the task boundary, logged,
   and the live state is retained because `ArcSwap::store` never
   ran.

The invariant is: either every component of `ReloadableState`
builds successfully, or none of it is swapped in.

## Dependencies

- `arc-swap` (new) — lock-free atomic Arc swap. Small crate, no
  unsafe in our usage, MIT/Apache-2.0.
- `notify` (new, opt-in) — only pulled in when `[reload] watch =
  true`. Cross-platform fs events.
- `tokio::signal::unix::signal(SignalKind::hangup())` (already
  available via the existing `tokio` dependency) — SIGHUP handler.

Both `arc-swap` and `notify` are pure-Rust, no C dependencies, and
compatible with the static-binary release story.

## Migration

Hot reload is opt-in via `[reload] enabled = true`. With the flag
unset (the default), `AppState` still wraps `ReloadableState` in
`ArcSwap` for uniform handler code, but no signal handler or
watcher is installed, and the swap path is never exercised. This
keeps the change behavior-preserving for existing deployments.

## Open questions

- **Reload metrics**: should successful and failed reloads be
  surfaced through a future `/metrics` endpoint, or is the audit
  log entry enough? The current direction is "audit log only" until
  a Prometheus exporter is proposed separately.
- **Audit log rotation**: a follow-up `SIGUSR1` handler could close
  and reopen the audit file for logrotate integration. Out of scope
  for this design.
- **Per-key partial reload**: reloading only the policy bundle
  without re-reading `nanoguard.toml` would be cheaper, but the
  policy bundle's keyword rules are merged into the matcher at
  build time, so the matcher has to be rebuilt anyway. Not worth
  the additional code path.
