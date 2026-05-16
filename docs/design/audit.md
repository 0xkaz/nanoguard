> **Status:** shipped (commit b217de5, 2026-05-17)

# Audit Log

nanoguard can emit an append-only JSONL audit trail for handled requests.
The audit path is intentionally simple:

- one line per request
- no database dependency
- optional hash-only prompt storage
- easy to ship with the proxy binary

## What Gets Logged

When audit logging is enabled, each entry records:

- request id
- timestamp
- api key
- model
- prompt hash
- verdict
- matched rule
- latency in microseconds

When the matched rule comes from a YAML policy bundle (see
[policy-engine.md](./policy-engine.md)), the entry also carries:

- `rule_id` (e.g. `PI-001`)
- `category` (e.g. `prompt_injection`)
- `severity` (e.g. `high`)
- `compliance` (array of regulatory tags)

These four fields are emitted only when populated, so audit entries from
deployments without a policy bundle keep the original shape. The
`shadow_block:` prefix on a demoted match is stripped before the policy
index is consulted, so shadow-mode entries also carry their rule
metadata.

The log does not need to retain raw prompt text to be useful.

## Privacy Posture

`hash_only = true` is the default.
That means nanoguard stores the SHA-256 hash of the prompt rather than the
prompt itself.

This is a privacy-oriented audit mode:

- enough to correlate events
- enough to prove something happened
- not enough to reconstruct sensitive content

## Durability Posture

Audit writes are **best-effort but never silent**.

- I/O errors on `writeln!` (disk full, read-only filesystem, broken
  fd, NFS write failure) are surfaced via `tracing::error!` instead of
  being dropped. The entry that triggered the failure is lost, but the
  operator sees a log line and can act on it.
- A poisoned mutex — for example, after another thread panicked while
  holding the audit lock — is **recovered** rather than skipped. The
  next write logs an error and proceeds. Without this, a single panic
  would silently disable the audit log for the rest of the process
  lifetime.
- Serialization failures are logged at `error` level and the entry is
  dropped. This should not happen for the schema defined above; if it
  does, the log line is the only signal.

For crash-durability, opt into `[audit] fsync_every_write = true`.
This calls `fsync` after each appended line, trading throughput for a
guarantee that flushed entries survive a host crash. The default
(`false`) lets the kernel flush on its own schedule, which is
appropriate for normal compliance / troubleshooting use.

## Scope

The audit log is meant for:

- compliance evidence
- troubleshooting
- policy rollout analysis
- request tracing

It is not a full SIEM or tamper-evident ledger.

## Limitations

The current implementation writes JSONL locally.
It does not yet provide:

- log shipping
- signed manifests
- WORM storage
- retention management

Those are separate operational concerns.

