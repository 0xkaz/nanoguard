> **Status:** shipped (v0.7.0, 2026-05-10)

# Proxy Contract

nanoguard is a transparent HTTP proxy for OpenAI-compatible and
Anthropic-compatible chat traffic. It filters a request before
forwarding, inspects the response (and any tool calls or structured
JSON it carries), and returns a sanitized result to the caller.

## Startup

Once at boot, nanoguard:

1. Reads `nanoguard.toml` (or environment overrides).
2. Loads any YAML policy bundle named in `[policies] bundle_path`,
   merges its keyword rules into the inline keyword config and its
   regex rules into the redactor pattern set.
3. Builds the matcher (aho-corasick by default), the redactor
   (entity-named patterns + per-entity action map), the optional
   spotlight transform, the optional output JSON Schema validator,
   and the optional tool gate. Each is stored on `AppState` only when
   its config block enables it.
4. Opens the audit log if `[audit] enabled = true`.

After startup the matcher / redactor are read-only on the request
hot path; the policy index is consulted only at audit time.

## Request Flow (`/v1/chat/completions`)

1. Parse the JSON body.
2. Extract `messages[].content` (string or `parts[].text`) for scanning.
3. **Input keyword guardrails** (`block` / `alert` / `flag` / `shadow`).
   `shadow = true` demotes a Block to a Flag with prefix
   `shadow_block:`.
4. **PII redaction** with per-entity actions:
   - `reject` short-circuits with HTTP 400.
   - `mask` rewrites `messages[].content` in place (string and
     `parts[].text` shapes both).
   - `log` records an alert and forwards unchanged.
   When `reversible = true`, the request-side Vault stores
   `(placeholder, original)` pairs for response-side restoration.
5. **Spotlighting**: tag content of any role in `untrusted_roles`
   (default `["tool"]`) so the LLM treats it as data, not as
   instructions. Three transforms: `delimiting`, `datamarking`,
   `encoding`. A method-specific system rider is prepended /
   appended to teach the model the convention.
6. **Budget check** (if enabled): refuse with HTTP 429 if the API key
   is over quota.
7. **Forward to backend**.
8. **Streaming response path**:
   - Buffered SSE filter splits `data: {...}\n\n` events across chunk
     boundaries.
   - The matcher's output filter runs against each event's
     `delta.content`.
   - The streaming Vault deanonymizer restores placeholders even when
     they straddle SSE events.
   - The streaming tool gate accumulates `tool_calls[]` deltas and,
     on `finish_reason: "tool_calls"`, evaluates the completed call;
     a Deny outcome emits a synthetic `tool_call_denied` SSE event
     followed by `[DONE]`.
   - Streaming usage (when `stream_options: {include_usage: true}`)
     is recorded against the budget store after the stream completes.
9. **Non-streaming response path**:
   - Output filter runs on `choices[].message.content`.
   - Vault deanonymizer restores PII placeholders if reversible mode
     was on.
   - Tool gate inspects each `tool_calls[]` entry: Allow / Deny
     (drop the call and surface it under
     `message.nanoguard_denied_tools`) / Sanitize (rewrite arguments).
   - JSON Schema validator picks the rule for
     `(/v1/chat/completions, model)` and either rejects, logs, or
     (future) repairs.
10. **Audit write**: the audit entry's `matched_rule` is enriched
    with `rule_id` / `category` / `severity` / `compliance` from the
    policy index when applicable.

## Anthropic Adapter (`/v1/messages`)

Anthropic requests are normalized to the OpenAI shape, run through
the same input pipeline (keyword scan, PII redaction, spotlight,
budget), forwarded, then converted back. The response path runs the
tool gate and the schema validator against the OpenAI-shaped backend
response, then re-shapes the surviving content into Anthropic blocks:
`text` blocks for assistant prose, `tool_use` blocks for surviving
tool calls, and a `nanoguard_denied_tools` block when anything was
rejected.

`tool_use` is supported on input and output. Vision and streaming for
`/v1/messages` are not yet supported.

## Supported Endpoints

- `POST /v1/chat/completions`
- `POST /v1/messages`
- `GET /v1/models`
- `GET /health`
- `GET /v1/admin/budget/:api_key`
- `PUT /v1/admin/budget/:api_key`
- `DELETE /v1/admin/budget/:api_key/reset`

## Determinism Guarantee

The proxy never calls an LLM to make a filtering decision. Filtering
is deterministic and local: aho-corasick / regex / JSON Schema, plus
declarative YAML policy bundles. LLM-based filtering remains an
explicit non-goal in the core path.

## Audit

Audit logging is optional. When enabled, every handled request writes
a JSONL entry. Required fields:

- request id, timestamp, api key, model, prompt hash, verdict,
  matched rule, latency in microseconds.

Optional fields populated when a policy bundle drove the match:

- `rule_id`, `category`, `severity`, `compliance`.

`hash_only = true` (default) stores the SHA-256 hash of the prompt
rather than the raw text.

## Non-Goals

- semantic moderation by an LLM judge in the core path
- adversarially robust prompt-injection defense (best-effort only)
- provider routing
- tokenization or model execution

nanoguard is a policy boundary, not a full AI platform.
