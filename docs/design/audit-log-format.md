> **Status:** shipped (commit 19fddf0, 2026-05-16)

# Audit Log Format

When `[audit] enabled = true`, nanoguard appends one JSON object per
line to `[audit] path`. The file is opened in append mode at startup
and never rotated by the process — operators rotate it externally
(`logrotate` with `copytruncate`, or a future `SIGUSR1` handler).

Two entry shapes share the file. They are distinguished by their
`verdict` field. Consumers should always read `verdict` first and
dispatch on it.

## Request entries

Written once per processed request that triggered a guardrail decision.

```json
{
  "request_id":  "000000000000000018adfc678fe51118",
  "timestamp":   "2026-05-09T19:29:09.607304+00:00",
  "api_key":     "alice-stream",
  "model":       "qwen3:0.6b",
  "prompt_hash": "d4c067494508331c38975ab3356c6f213c1f571969d3d2c1ad8d1c45d1bc20db",
  "verdict":     "block",
  "matched_rule": "jailbreak",
  "rule_id":     "PI-001",
  "category":    "prompt_injection",
  "severity":    "high",
  "compliance":  ["GDPR", "HIPAA"],
  "latency_us":  102
}
```

### Field reference

| Field          | Type             | Always present | Description                                                                                                                                          |
|----------------|------------------|----------------|------------------------------------------------------------------------------------------------------------------------------------------------------|
| `request_id`   | string           | yes            | 32-char hex of the nanosecond timestamp at request receive.                                                                                          |
| `timestamp`    | string (RFC3339) | yes            | UTC timestamp when the audit entry was written.                                                                                                      |
| `api_key`      | string           | yes            | The OpenAI `user` field from the request body, or `"default"`. Will become the verified client-token prefix once `feat/client-auth` lands.           |
| `model`        | string           | yes            | The `model` field from the request body, or `"unknown"`.                                                                                             |
| `prompt_hash`  | string           | yes            | SHA-256 hex digest of the concatenated user-message text. When `hash_only = true` (the default) this is the only record of the prompt.               |
| `verdict`      | string           | yes            | `"allow"` or `"block"`. `"block"` means the request was rejected before reaching the backend.                                                        |
| `matched_rule` | string \| null   | yes            | The phrase or pattern that triggered the decision. For shadow-mode demotions: prefixed `shadow_block:`.                                              |
| `rule_id`      | string           | when policy    | Policy-bundle rule id (e.g. `"PI-001"`). Omitted from JSON when the match did not come from a policy.                                                |
| `category`     | string           | when policy    | Policy-bundle category (e.g. `"prompt_injection"`, `"pii"`).                                                                                         |
| `severity`     | string           | when policy    | Policy-bundle severity: `"low"` / `"medium"` / `"high"` / `"critical"`.                                                                              |
| `compliance`   | array\<string\>  | when policy    | Compliance frameworks attached to the matched rule (e.g. `["GDPR", "HIPAA"]`). Omitted from JSON when empty.                                         |
| `latency_us`   | u64              | yes            | Microseconds spent inside the guardrail pipeline. Does not include backend latency.                                                                  |

### When entries are emitted

- A `block` verdict is written whenever an input guardrail rejects
  the request (keyword block, PII reject-class entity, schema reject
  mode). The request never reaches the backend.
- An `allow` verdict is written on the normal request-completed
  path. The `matched_rule` carries the most-recent matched alert /
  flag if any, otherwise `null`.

Alert and flag verdicts are surfaced as `tracing::info` log lines,
not as separate audit entries — only `block` and `allow` reach the
audit JSONL.

### `hash_only` mode

When `[audit] hash_only = true` (the default), nanoguard stores
only `prompt_hash`, never the raw prompt text. This is appropriate
for compliance environments where retaining user input is
restricted. The hash is sufficient to prove "the same prompt was
seen" without persisting the prompt itself.

When `hash_only = false`, future versions may add a `prompt` field
alongside `prompt_hash`. As of this commit, `hash_only = false` is
recognized in config but the codebase has no path that writes the
plaintext field — the flag is reserved for forward compatibility.
Consumers should treat the absence of a `prompt` field as expected.

## Reload entries

Written once per SIGHUP-triggered reload attempt (see
`docs/design/hot-reload.md`).

```json
{
  "request_id": "000000000000000018b00b419c7c7d90",
  "timestamp":  "2026-05-16T12:23:49.237537+00:00",
  "verdict":    "reload_ok",
  "latency_us": 1287
}
```

```json
{
  "request_id": "000000000000000018b00b419c7c7d90",
  "timestamp":  "2026-05-16T12:23:49.237537+00:00",
  "verdict":    "reload_failed",
  "error":      "config_parse_error",
  "latency_us": 82
}
```

### Field reference

| Field        | Type             | Always present | Description                                                                                                  |
|--------------|------------------|----------------|--------------------------------------------------------------------------------------------------------------|
| `request_id` | string           | yes            | Same 32-char-hex shape as request entries. Reloads are not requests, but they reuse the id generator.        |
| `timestamp`  | string (RFC3339) | yes            | UTC timestamp when the reload attempt completed.                                                             |
| `verdict`    | string           | yes            | `"reload_ok"` or `"reload_failed"`.                                                                          |
| `error`      | string           | on failure     | Bounded category label, **not** raw error text — see "Error label vocabulary" below. Omitted on `reload_ok`. |
| `latency_us` | u64              | yes            | Microseconds spent rebuilding the candidate state.                                                           |

### Error label vocabulary

`error` is intentionally a bounded enum-style string, not a free-form
message. The full anyhow chain (which can include config / pattern /
schema content) is kept in `tracing::debug` only. Current labels:

| Label                       | Meaning                                                                       |
|-----------------------------|-------------------------------------------------------------------------------|
| `config_parse_error`        | `nanoguard.toml` failed to parse.                                             |
| `policy_yaml_parse_error`   | The policy YAML bundle failed to parse.                                       |
| `regex_compile_error`       | A regex pattern (in a dict or policy) failed to compile.                      |
| `json_parse_error`          | A JSON file referenced by config (tool schemas, etc.) failed to parse.        |
| `file_not_found`            | A referenced file (dict, schema, policy) was missing on disk.                 |
| `file_permission_denied`    | A referenced file existed but could not be opened.                            |
| `io_error`                  | Any other filesystem I/O failure during reload.                               |
| `build_failed`              | Any other failure in the reload pipeline — catch-all.                         |

Consumers may safely treat unknown labels as `build_failed` for
alerting purposes. New labels will be added under semver-minor.

## Differentiating the two shapes

A `verdict` value tells the consumer which shape to expect:

- `"allow"` or `"block"` → request entry (full schema above).
- `"reload_ok"` or `"reload_failed"` → reload entry (slim schema).

The slim shape never carries `api_key` / `model` / `prompt_hash` —
a reload is not a request. Consumers that index by `api_key`
should filter on `verdict in ("allow", "block")` first.

## Stability

Field additions to either shape are non-breaking by convention:

- New optional fields are appended; existing readers ignore them.
- The set of `verdict` values may grow (e.g. `"reload_partial"`
  when restart-only keys are touched). Readers should treat
  unknown verdicts as opaque rather than failing.
- Field removals are breaking and require a major-version bump.

Field types do not change in place. If `latency_us` ever needs to
go past u64 (it will not), the change ships as a new field, not as
a redefinition.

## Reading the log

JSONL is line-delimited JSON, one record per line. Standard tools:

```bash
# Watch all decisions live
tail -f nanoguard-audit.jsonl | jq

# Just the blocks from a specific api_key
jq -c 'select(.verdict == "block" and .api_key == "alice")' nanoguard-audit.jsonl

# Reload outcomes
jq -c 'select(.verdict | startswith("reload_"))' nanoguard-audit.jsonl

# Latency percentiles across allow + block (skip reloads)
jq -s '[.[] | select(.verdict == "allow" or .verdict == "block") | .latency_us]
       | sort | length as $n
       | {p50: .[$n/2|floor], p95: .[$n*0.95|floor], p99: .[$n*0.99|floor]}' \
  nanoguard-audit.jsonl
```
