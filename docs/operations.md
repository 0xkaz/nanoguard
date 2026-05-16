# Operations Runbook

Operator-facing how-to for running nanoguard in a real deployment.
This document is **not a design spec** — design lives under
`docs/design/`. This is the "what do I actually do on the box"
reference.

## Process model

nanoguard is a single binary, single process. It listens on the
address from `[nanoguard] listen` (default `0.0.0.0:8080`) and
forwards to a single backend defined by `[backend]`. Run it under
your platform's standard process supervisor — systemd, Docker, k8s,
runit. The binary does not daemonize itself.

### Recommended systemd unit

```ini
[Unit]
Description=nanoguard LLM guardrails proxy
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/nanoguard
Environment=NANOGUARD_CONFIG=/etc/nanoguard/nanoguard.toml
Environment=RUST_LOG=info
# Required so SIGHUP (see "Hot reload" below) reaches the process,
# not systemd's reload semantics.
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
RestartSec=2s
# Drop privileges; nanoguard does not need root.
DynamicUser=yes
StateDirectory=nanoguard
WorkingDirectory=/var/lib/nanoguard

[Install]
WantedBy=multi-user.target
```

The `ExecReload=/bin/kill -HUP $MAINPID` line is what lets you run
`systemctl reload nanoguard` and have it trigger a hot reload (see
below) rather than a full process restart.

## Hot reload

SIGHUP triggers an atomic rebuild of the request-side configuration:
keyword matcher, PII redactor, spotlight transform, JSON Schema
validators, Tool Gate, policy bundle. The new state replaces the
live one with a single atomic pointer swap; in-flight requests
finish on the snapshot they acquired at request entry.

```bash
# Edit any of:
#   - /etc/nanoguard/nanoguard.toml
#   - dict files referenced by [input.keyword] dict_paths
#   - the policy bundle referenced by [policies] bundle_path

# Signal nanoguard to pick up the changes
sudo kill -HUP $(pidof nanoguard)
# or, under systemd:
sudo systemctl reload nanoguard
```

### What gets reloaded

- `[input.*]` — keyword rules, PII patterns, spotlight, normalize
- `[output.*]` — output filter, JSON Schema rules
- `[tools]` — Tool Gate allow / deny / schemas / entity sets
- `[policies] bundle_path` — the full policy YAML and everything
  it merges into

### What is restart-only

A reload that touches any of these keys logs them as ignored; the
reload of other keys still proceeds.

| Key                       | Why restart-only                                              |
|---------------------------|---------------------------------------------------------------|
| `[nanoguard] listen`      | The TcpListener is bound once at startup.                     |
| `[nanoguard] log_level`   | The tracing subscriber is installed once.                     |
| `[backend] *`             | `reqwest::Client` owns a connection pool; swapping would orphan keep-alives. |
| `[budget] db_path`        | SQLite connection + WAL state.                                |
| `[audit] path`            | Open file handle in append mode.                              |

To change any of these, do a full restart:

```bash
sudo systemctl restart nanoguard
```

### Validation

Reload is **all-or-nothing**. If the new config fails to parse, a
dict has a regex that won't compile, or a JSON Schema is invalid,
the live state is retained and the audit log gets a
`reload_failed` entry. No partial state ever reaches request
handlers.

To verify a successful reload:

```bash
# Wait briefly after sending SIGHUP, then check the audit log
tail -n 5 /var/lib/nanoguard/nanoguard-audit.jsonl \
  | jq -c 'select(.verdict | startswith("reload_"))'

# Expected for success:
# {"verdict":"reload_ok","latency_us":1287,...}

# On failure, get a bounded error label:
# {"verdict":"reload_failed","error":"config_parse_error",...}
```

The `error` label is one of a fixed vocabulary (see
[audit-log-format.md](design/audit-log-format.md) for the full
list). The full error chain is in `tracing::debug` only — bring up
the log level temporarily to diagnose:

```bash
sudo systemctl edit nanoguard
# add: Environment=RUST_LOG=debug
sudo systemctl restart nanoguard
sudo kill -HUP $(pidof nanoguard)
journalctl -u nanoguard -f
```

## Audit log

When `[audit] enabled = true`, every guardrail decision and every
reload outcome appends one JSON object to `[audit] path`. See
[audit-log-format.md](design/audit-log-format.md) for the full
schema.

### Rotation

nanoguard holds the audit file open in append mode for the process
lifetime. It does not rotate the file itself. Use `logrotate` with
`copytruncate`:

```conf
# /etc/logrotate.d/nanoguard
/var/lib/nanoguard/nanoguard-audit.jsonl {
    daily
    rotate 14
    compress
    delaycompress
    missingok
    notifempty
    copytruncate
}
```

`copytruncate` is mandatory because nanoguard does not handle
`SIGUSR1` for reopen-on-signal (yet). With `copytruncate`,
logrotate copies the current contents to the rotated name and then
truncates the original file in place — nanoguard's open handle
continues writing to (now-empty) inode without noticing.

### Shipping

The JSONL is line-delimited and append-only. Standard log shippers
work without modification: Vector, Fluent Bit, Promtail, Filebeat.
The file does not move under nanoguard's feet (it only grows, then
gets truncated by logrotate), so tail-followers stay attached.

If you ship to a SIEM, a useful index:

- Primary key: `request_id` (unique per request and per reload)
- Tenant key: `api_key`
- Severity key: `severity` (when present; from policy bundle)
- Time key: `timestamp`

## Backend configuration

`[backend]` is restart-only. Change it like this:

```bash
sudo vi /etc/nanoguard/nanoguard.toml   # edit [backend]
sudo systemctl restart nanoguard
```

nanoguard does NOT probe the backend at startup; it will happily
come up and accept requests even if the backend is unreachable.
Failures surface as `502 Bad Gateway` on the first forwarded
request. Check `journalctl -u nanoguard -f` if requests start
failing.

### Switching providers

To move from OpenAI to a self-hosted Ollama (or any OpenAI-compatible
provider) without changing client code:

```toml
# /etc/nanoguard/nanoguard.toml — old
[backend]
provider = "openai"
endpoint = "https://api.openai.com"
api_key  = "${OPENAI_API_KEY}"
model    = "gpt-4o-mini"

# new
[backend]
provider = "ollama"
endpoint = "http://localhost:11434"
model    = "qwen3:0.6b"
```

Restart. Clients keep calling `/v1/chat/completions` on nanoguard;
nanoguard now fans out to Ollama.

Multi-backend routing (configuring several backends and per-user
allowed models) is `proposed` in
[multi-backend-routing.md](design/multi-backend-routing.md) — not
yet implemented as of this commit.

## Budget admin API

When `[budget] enabled = true` and `admin_api_key` is set, three
endpoints are available:

```bash
# Get usage + limit for an API key (the OpenAI `user` field)
curl -H "Authorization: Bearer $ADMIN_API_KEY" \
  http://localhost:8080/v1/admin/budget/alice

# Set a token limit
curl -X PUT -H "Authorization: Bearer $ADMIN_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"limit": 100000}' \
  http://localhost:8080/v1/admin/budget/alice

# Reset usage counter
curl -X DELETE -H "Authorization: Bearer $ADMIN_API_KEY" \
  http://localhost:8080/v1/admin/budget/alice/reset
```

The admin API is a separate auth scope from the proxy endpoints —
the admin Bearer never reaches the backend.

## Health checking

`GET /health` is unauthenticated and returns `ok` when the process
is alive. Use it as a Kubernetes liveness probe / load-balancer
health check. It does NOT verify backend connectivity — nanoguard
does not probe the backend at startup either (see "Backend
configuration" above), so backend reachability surfaces only when
a real request is forwarded (502 on failure). Pair `/health` with
a synthetic chat-completion call against a known model if you
need backend-aware health.

For deeper monitoring, watch the audit log:

- Sustained `verdict: "block"` rate above some threshold → either an
  attack or a too-aggressive rule set.
- `reload_failed` entry → operator pushed a bad config; alert
  immediately.
- p99 `latency_us` rising → the rule set is getting expensive (large
  regex set, long dict). Profile with `make bench`.

## Common issues

### SIGHUP arrives but nothing changes

Causes (in order of frequency):

1. The config file was edited but `NANOGUARD_CONFIG` points
   somewhere else. Check `journalctl -u nanoguard -n 50 | grep -i
   config`.
2. The reload silently failed with `reload_failed`. Check the audit
   log:
   ```bash
   tail -n 20 /var/lib/nanoguard/nanoguard-audit.jsonl | jq -c '. | select(.verdict == "reload_failed")'
   ```
3. The process you signaled was not nanoguard. Verify with `pidof
   nanoguard`.

### Audit log grows without rotation

logrotate is not running, or it's configured without `copytruncate`.
A `truncate -s 0 /path/to/audit.jsonl` works in a pinch but is
crude; install the logrotate snippet above instead.

### Backend timeout under load

`[backend]` does not currently expose a configurable timeout —
nanoguard uses `reqwest`'s default (no explicit timeout; relies on
the OS). nanoguard does not retry; that's the client's
responsibility. A configurable `[backend] timeout_secs` is in the
`multi-backend-routing.md` proposed design and will land with that
work.
