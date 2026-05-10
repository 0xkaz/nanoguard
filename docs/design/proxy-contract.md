> **Status:** shipped (commit 82d65f3, 2026-05-10)

# Proxy Contract

nanoguard is a transparent HTTP proxy for OpenAI-compatible chat traffic.
It filters a request before forwarding it, then filters the response before
returning it to the caller.

## Request Flow

1. Parse the JSON body.
2. Extract `messages[].content` for keyword scanning.
3. Apply input guardrails.
4. Apply PII redaction or rejection.
5. Check budget, if enabled.
6. Forward the request to the configured backend.
7. Filter the backend response.
8. Record audit data.

The proxy never calls an LLM to make a filtering decision.
Filtering is deterministic and local.

## Supported Endpoints

- `POST /v1/chat/completions`
- `POST /v1/messages`
- `GET /v1/models`
- `GET /health`
- `GET /v1/admin/budget/:api_key`
- `PUT /v1/admin/budget/:api_key`
- `DELETE /v1/admin/budget/:api_key/reset`

## Input Guardrails

Input guardrails run before the backend call.
They can:

- `block` known injection phrases
- `alert` on lower-severity matches
- `flag` on off-topic or policy-relevant matches
- `shadow` scan without blocking

`shadow = true` keeps audit visibility while allowing the request through.
This is useful when rolling out a new rule set and measuring false positives.

## Output Guardrails

Output filtering runs on:

- non-streaming `choices[].message.content`
- streaming SSE `choices[].delta.content`

Streaming is chunk-aware and event-aware.
The current implementation buffers SSE data across chunk boundaries before
filtering each event, then forwards the filtered stream.

## Audit

Audit logging is optional.
When enabled, every handled request writes a JSONL entry containing:

- request id
- timestamp
- api key
- model
- prompt hash
- verdict
- matched rule
- latency in microseconds

`hash_only = true` means the audit log stores the SHA-256 hash of the prompt
instead of the raw prompt text.

## Non-Goals

- semantic moderation
- adversarially robust prompt-injection defense
- provider routing
- tokenization or model execution

nanoguard is a policy boundary, not a full AI platform.

