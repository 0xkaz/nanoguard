> **Status:** shipped (v0.5.0 base, v0.6.0 streaming + Anthropic, last revised 2026-05-11)

# Tool Gate

The Tool Gate inspects each LLM-emitted tool / function call before the
application can execute it. It is the layer that turns nanoguard from
"a proxy that filters text" into "a proxy that controls side effects."

## Why It Exists

Once an LLM is allowed to emit `tool_calls`, the danger of a prompt
injection is no longer just "wrong text comes out." A model that has
been talked into deleting tickets, sending emails, or paging out an
on-call engineer can actually do those things — the damage is real
and immediate.

Text-side guardrails (keyword scan, PII redaction, output filter) do
not catch this. By the time a tool call exists, the request body
that produced it is already past every prompt-side check.

The Tool Gate sits at the *response* boundary, between the LLM and
the calling application, and decides whether each tool call is allowed
to flow through.

## What It Inspects

For every tool call that appears in the response, the gate runs three
layered checks:

1. **Name** — allow / deny by tool name with `*` wildcards.
2. **Arguments shape** — JSON Schema validation of the call's
   arguments, when a schema is registered for the tool name.
3. **Argument contents** — PII / secret scan on the argument JSON
   string using the existing request-side `Redactor`. Two configurable
   entity sets:
   - `reject_entities` → any match denies the call outright.
   - `mask_entities` → matches trigger an in-place mask, returning
     a `Sanitize` decision.

The check order matters: name → schema → entities. A call rejected on
name never has its arguments parsed; a malformed arguments JSON is
denied before any entity scan runs.

## Decisions

```rust
pub enum ToolDecision {
    Allow,
    Deny { reason: String },
    Sanitize { redacted_args: Value },
}
```

`Allow` lets the call through unchanged. `Deny` removes the call
from the response and surfaces the reason. `Sanitize` rewrites
the arguments in place and lets the call through.

`Sanitize` is only meaningful on the non-streaming path. On a
streaming response the argument deltas have already been forwarded
to the client by the time the full call is assembled, so the
streaming gate (`src/guard/sse_tool_gate.rs`) treats Sanitize
outcomes as Allow and surfaces denials as a synthetic
`tool_call_denied` SSE event followed by `[DONE]`.

## Endpoint Coverage

Both endpoints route their non-streaming responses through the gate:

- `POST /v1/chat/completions` (`src/proxy/mod.rs`): denied calls
  are removed from `choices[].message.tool_calls` and surfaced under
  `choices[].message.nanoguard_denied_tools`.
- `POST /v1/messages` (`src/proxy/anthropic.rs`): the OpenAI-shaped
  response is gated, then surviving `tool_calls` are reshaped into
  Anthropic `{"type": "tool_use"}` content blocks. Denials become a
  `{"type": "nanoguard_denied_tools", "details": [...]}` block.

Streaming gate is wired only on `/v1/chat/completions`. Anthropic
streaming on `/v1/messages` is not supported in v0.7.0 and is
refused with HTTP 400.

## Configuration

```toml
[tools]
enabled = false
allow = ["search_*", "read_*"]   # None = open-mode (deny list only)
deny  = ["delete_*", "shell_exec"]
reject_entities = ["AWS_ACCESS_KEY_ID", "JWT", "ANTHROPIC_KEY"]
mask_entities   = ["EMAIL", "PHONE"]

[[tools.schemas]]
tool_name = "send_email"
schema_path = "schemas/send_email.json"
```

Allow / deny are independent: an empty `allow` means "open mode"
(allow everything not on `deny`); a non-empty `allow` means
"closed mode" (only items on `allow` pass, then `deny` still wins).

The Redactor used for entity scanning is the same one configured
under `[input.pii]`. Entities listed in `reject_entities` /
`mask_entities` must be names the Redactor knows about
(see `default_inline_patterns()` in `src/proxy/redact.rs` and any
custom dicts loaded via `input.pii.dict_paths`).

## Pipeline Position

```
LLM response
  ├─ non-streaming path
  │     ↓
  │   tool gate (apply_tool_gate)
  │     ↓
  │   schema validator
  │     ↓
  │   reversible PII deanonymize
  │     ↓
  │   client
  │
  └─ streaming path (chat_completions only)
        ↓
      SseFilter chunk-buffer
        ↓
      streaming tool gate (delta accumulator)
        ↓
      output filter on delta.content
        ↓
      streaming deanonymize
        ↓
      client
```

Tool gate runs *before* schema validation so a denied call cannot
trigger schema rejection of an empty `tool_calls` array.

## What It Does Not Do

- It does not call the network. Every check is offline / regex /
  in-process JSON validation.
- It does not modify message content (only `tool_calls` and the
  bookkeeping `nanoguard_denied_tools` field).
- It does not rate-limit calls. Allow lists are a yes/no gate, not
  a budget.
- It does not enforce ordering between tool calls. A schema that
  requires "validate_user before send_payment" needs a higher-level
  policy engine; the gate operates one call at a time.

## Limitations

- Streaming Sanitize is degraded to Allow (see "Decisions"). A
  buffered streaming mode that holds the full delta sequence before
  forwarding any of it would close the gap at the cost of latency.
- Anthropic streaming is currently refused at the endpoint, so the
  streaming gate has no Anthropic counterpart yet.
- Schema validators are compiled once at startup; reloading a tool's
  schema requires a restart.

## See Also

- `src/guard/tool_gate.rs` — the gate itself
- `src/guard/sse_tool_gate.rs` — streaming accumulator
- `docs/design/json-schema.md` — argument schema validation, the same
  crate the gate's schema layer uses
- `docs/design/proxy-contract.md` — overall request flow including
  where the gate sits
