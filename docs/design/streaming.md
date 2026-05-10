> **Status:** shipped (commit 82d65f3, 2026-05-10)

# Streaming

nanoguard supports streaming chat responses and applies filtering per SSE
event rather than per TCP chunk.

## Why This Exists

OpenAI-compatible backends do not guarantee a 1:1 mapping between TCP chunks
and SSE events.
A backend may:

- split one event across multiple chunks
- pack multiple events into one chunk

Filtering at the chunk layer would be unreliable.

## Current Behavior

The streaming path:

1. buffers incoming bytes
2. splits on the SSE event terminator
3. filters `choices[].delta.content` inside each event
4. forwards non-data lines and `[DONE]` unchanged
5. opportunistically extracts usage metadata when present

This keeps the proxy behavior deterministic while preserving streaming.

## Usage Accounting

Streaming usage accounting depends on the backend emitting usage metadata.
If the backend includes an OpenAI-compatible `usage` object, nanoguard can
record spend after the stream completes.

If usage metadata is missing, the stream still works, but spend accounting is
not available for that response.

## Deanonymization

When reversible PII redaction is enabled, the streaming path can restore
placeholders that are split across SSE event boundaries.
That is handled by a placeholder-aware state machine after output filtering.

## Limitations

Streaming output filtering is still event-local for plain text content.
If a sensitive token is genuinely split across two SSE events, the matcher
may not detect it.

That limitation is intentional and documented.
For higher-assurance deployments, combine nanoguard with full-response
scanning or buffering mode.

