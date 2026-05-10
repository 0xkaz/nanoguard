> **Status:** shipped (v0.5.0, last revised 2026-05-11)

# Output JSON Schema Validation

nanoguard can validate the LLM's textual response against a JSON
Schema (Draft 2020-12) and either log, reject, or eventually repair
the payload when it fails. The same validation engine backs the
Tool Gate's argument-shape check.

## Why It Exists

A surprising amount of production LLM traffic is structured: tool
arguments, JSON-mode replies, function-calling outputs, ETL
extractions. The application code on the other end of the pipe is
usually written assuming the JSON conforms to a schema, and crashes
or silently misroutes when it does not.

Validating at the proxy boundary has two benefits:

1. The validation policy is centralized — every consumer of the
   route gets the same shape guarantee, without each app re-deriving
   it.
2. The audit log records validation outcomes, so you can answer
   "how often does this model hallucinate the wrong shape?" with a
   query, not a hand-rolled probe.

## What Gets Validated

For non-streaming responses on `POST /v1/chat/completions` and
`POST /v1/messages`, nanoguard:

1. Picks the first `SchemaRule` whose `endpoint` matches and whose
   optional `model_pattern` regex matches the request's `model` field.
2. Pulls the assistant's textual `content`.
3. Strips a leading prose preamble or a markdown ```json fenced
   block, if either is wrapping the JSON.
4. Parses the cleaned text as JSON.
5. Runs the JSON Schema validator over it.

The wrapper-stripping is intentionally conservative: it strips a
matched fence only when the entire response is fence-wrapped, and
only trims prose up to the first `{` or `[`. Plain JSON responses
go through unchanged.

## Violation Actions

```rust
pub enum ViolationAction {
    Reject,    // 502-equivalent error to the caller
    LogOnly,   // (default) audit-record the violation, pass through
    Repair,    // reserved; currently behaves like LogOnly
}
```

Mapped from `output.schema.on_violation` in TOML:

```toml
[output.schema]
enabled = true
on_violation = "log"   # "reject" | "log" | "repair"
```

`Repair` is reserved for a future port of `json_repair` /
`llm_json` — the goal is "drop a missing comma, coerce a string
that should be an int" — but the reserved name is shipped now so
deployments don't have to migrate config later. Until that lands,
`Repair` is identical to `LogOnly` plus a warning in the log.

`Reject` returns HTTP 400 with the Schema rule's name and the
validator's error chain in the response body.

## Rules

```toml
[[output.schema.rules]]
endpoint      = "/v1/chat/completions"
model_pattern = "gpt-4o.*"
schema_path   = "schemas/user_card.json"
name          = "user_card"

[[output.schema.rules]]
endpoint    = "/v1/chat/completions"
schema_path = "schemas/default.json"
name        = "default"
```

Rules are evaluated top-down; the first match wins. A rule without
`model_pattern` matches any model on that endpoint and is the place
to put a "default shape for this route" rule (always last).

Schemas are compiled once at startup with `jsonschema::draft202012`.
Reloading a schema requires a restart.

## Tool Gate Reuse

The Tool Gate (`docs/design/tool-gate.md`) uses the same `jsonschema`
crate to validate tool-call arguments against per-tool schemas:

```toml
[[tools.schemas]]
tool_name   = "send_email"
schema_path = "schemas/send_email.json"
```

The two surfaces (response body, tool call arguments) are kept in
separate config blocks because they target different layers, but
the underlying validator behavior is identical.

## What It Does Not Do

- It does not enforce schemas on streaming responses. The streaming
  filter forwards SSE events as they arrive; by the time the full
  JSON exists, the client has already consumed most of it. A
  buffered-streaming validation mode is on the roadmap.
- It does not coerce types. A field declared `integer` that arrives
  as `"42"` (string) is a violation, not a quiet conversion.
- It does not validate against TypeScript types, OpenAPI specs, or
  Pydantic models — only JSON Schema. Most other formats can be
  exported to JSON Schema externally.

## Pipeline Position

```
non-streaming response
  ↓
tool gate
  ↓
schema validator    ← here
  ↓
reversible deanonymize
  ↓
client
```

Schema validation runs after the tool gate so it sees the
post-gate `tool_calls` array (or its absence). It runs before
reversible deanonymize so the schema sees placeholders, not
restored PII — applications using reversible mode should write
schemas against the placeholder shape.

## Limitations

- Streaming responses are not validated.
- `Repair` is a stub.
- Only Draft 2020-12 is supported. Older drafts are accepted by the
  validator but not formally supported.
- Only the OpenAI-shaped `choices[].message.content` field is
  validated; structured replies in other shapes (Anthropic content
  blocks beyond the textual one) are not.

## See Also

- `src/guard/schema.rs` — implementation and unit tests
- `docs/design/tool-gate.md` — argument-shape validation reuses
  the same `jsonschema` crate
- `docs/design/proxy-contract.md` — pipeline placement
