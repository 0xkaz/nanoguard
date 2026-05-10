> **Status:** proposed (review matrix, last revised 2026-05-10)

# Test Matrix

This document is a review matrix for behavior that spans multiple agents and
multiple layers of the proxy. It is not a unit test spec and it is not a
roadmap item list.

The goal is to keep new work from drifting across boundaries:

- keyword guardrails
- redaction
- budget tracking
- audit logging
- streaming output filtering
- schema validation
- tool gating
- policy engine
- recognizer evaluation
- Anthropic and OpenAI compatibility

## Review Rules

For each row, verify:

1. the input shape
2. the expected decision
3. the failure mode
4. the relevant module
5. the docs that describe the behavior

When a row fails, the failure should be obvious from a test, a benchmark, or a
request/response trace.

## Matrix

### 1. Input Guardrails

| Case | Input shape | Expected result | Failure mode | Related module |
|---|---|---|---|---|
| Known prompt injection phrase | Plain text message contains a literal rule hit | `block` | Silent allow or stale rule set | `src/matcher`, `src/proxy` |
| Lower severity policy term | Plain text message contains a policy term | `alert` or `flag` | Over-blocking or no audit trail | `src/matcher`, `src/audit` |
| Shadow mode | Rule hit with `input.shadow = true` | Forward request, record shadow verdict | Request blocked in shadow mode | `src/matcher`, `src/proxy` |
| Normalized obfuscation | Zero-width, mixed case, leet, or NFKC variant | Same verdict as canonical form | Missed match after normalization | `src/matcher` |

### 2. PII Redaction

| Case | Input shape | Expected result | Failure mode | Related module |
|---|---|---|---|---|
| Email address | Request or tool argument includes an email | Mask, reject, or log depending on policy | Plaintext leakage | `src/proxy`, `src/guard` |
| SSN-like value | Request contains a structured SSN pattern | Mask or reject | False negative on formatted input | `src/proxy`, `src/guard` |
| Credit card-like value | Request contains a payment card pattern | Mask or reject | Luhn-like pattern missed | `src/proxy`, `src/guard` |
| Reversible redaction | Redaction is enabled with vault backing | Placeholder appears in forwarded text, original is recoverable only through vault path | Irreversible placeholder or accidental plaintext forwarding | `src/guard/vault`, `src/proxy` |
| Mixed content | Text plus JSON or tool payload | Only text segments are rewritten | Non-text content mutated or dropped | `src/proxy` |

### 3. Budget Tracking

| Case | Input shape | Expected result | Failure mode | Related module |
|---|---|---|---|---|
| Under budget | Request key has usage below limit | Request allowed | False positive budget reject | `src/budget`, `src/proxy` |
| Over budget | Request key exceeds configured limit | HTTP 429 with budget error | Request forwarded anyway | `src/budget`, `src/proxy` |
| Missing `user` field | OpenAI-style request omits user | Falls back to default budget key | Unbounded per-user accounting | `src/budget`, `src/proxy` |
| Non-streaming usage | Backend returns `usage` in final response | Spend is recorded after response | Spend never recorded | `src/budget`, `src/proxy` |
| Streaming usage | Backend emits `stream_options.include_usage = true` | Final usage chunk is recorded | Streaming request is unaccounted | `src/budget`, `src/proxy` |

### 4. Audit Logging

| Case | Input shape | Expected result | Failure mode | Related module |
|---|---|---|---|---|
| Hash-only mode | Audit enabled with `hash_only = true` | Prompt hash written, not raw prompt | Raw prompt stored unexpectedly | `src/audit` |
| Full audit mode | Audit enabled without hash-only | Structured JSONL entry written | Missing verdict or rule id | `src/audit` |
| Shadow verdict | Request would have been blocked | Audit record keeps `verdict = allow` and marks `matched_rule = shadow_block:...` | Shadow verdict missing from audit trail or not distinguishable in logs | `src/audit`, `src/proxy` |
| Audit disabled | Audit switched off | No audit file write | Hidden file write or stale entry | `src/audit` |

### 5. Streaming Output

| Case | Input shape | Expected result | Failure mode | Related module |
|---|---|---|---|---|
| SSE split across TCP chunks | One SSE event arrives in multiple reads | Event is buffered and filtered correctly | Chunk boundary breaks filtering | `src/proxy/sse.rs` |
| Multiple SSE events in one chunk | Several events arrive together | Each event is split and filtered independently | Event merge or dropped data | `src/proxy/sse.rs` |
| Split sensitive token | Token spans two SSE events | Current limitation is documented and test-covered | False negative without acknowledgment | `docs/design/streaming.md`, `src/proxy/sse.rs` |
| Usage metadata in final chunk | Final chunk includes usage fields | Usage is captured and forwarded to budget store | Stream completes without spend accounting | `src/budget`, `src/proxy/sse.rs` |
| Deanonymization state | Reversible placeholders span stream chunks | Placeholder is restored only when complete | Partial restore or leaked placeholder | `src/guard/sse_deanon.rs`, `src/proxy` |

### 6. Tool Gate

| Case | Input shape | Expected result | Failure mode | Related module |
|---|---|---|---|---|
| Allowed tool name | Tool call matches allow list and schema | Tool call passes | Safe tool denied | `src/guard/tool_gate.rs` |
| Denied tool name | Tool call matches deny list | Tool call rejected | Unsafe tool executed | `src/guard/tool_gate.rs` |
| Invalid schema | Tool arguments fail schema validation | Tool call rejected or sanitized | Invalid payload reaches executor | `src/guard/tool_gate.rs`, `src/guard/schema.rs` |
| PII in tool args | Tool arguments contain email/secret pattern | Tool call redacted or denied | Side effect leaks secret | `src/guard/tool_gate.rs`, `src/proxy` |
| Streaming tool call | Tool call is assembled from deltas | Decision waits until tool call is complete | Partial tool call evaluated too early | `src/proxy`, `src/guard/tool_gate.rs` |

### 7. Schema Validation

| Case | Input shape | Expected result | Failure mode | Related module |
|---|---|---|---|---|
| Valid JSON payload | Structured response matches schema | Pass | False negative on valid object | `src/guard/schema.rs` |
| Invalid JSON payload | Required field missing or wrong type | Fail | Invalid payload accepted | `src/guard/schema.rs` |
| Wrapper text present | Model adds prose around JSON | Wrapper is stripped or rejected per mode | Wrapper accepted as payload | `src/guard/schema.rs` |
| Model-specific rule | Schema chosen by endpoint/model pair | Correct schema applied | Wrong rule selected silently | `src/guard/schema.rs` |

### 8. Policy Engine and Recognizer

| Case | Input shape | Expected result | Failure mode | Related module |
|---|---|---|---|---|
| Policy bundle match | Policy rule set selects a path | Deterministic allow/block/redact result | Policy not applied or wrong precedence | `src/policy` if present, otherwise policy layer docs |
| Recognizer true positive | Recognizer detects the intended class | Evaluation marks success | Missed detection | `_RECOGNIZER_EVAL.md`, recognizer code |
| Recognizer false positive | Recognizer over-matches benign text | Evaluation marks overreach | False confidence from eval | `_RECOGNIZER_EVAL.md`, recognizer code |
| Anthropic tool use | `tool_use` or equivalent appears in Anthropic flow | Tool gate still applies | OpenAI-only logic misses Anthropic path | Anthropic adapter, tool gate |

## Minimum Pass Criteria

A feature is ready to merge when:

- the direct behavior is covered by at least one test
- the failure mode is explicit
- the relevant design doc matches the implementation
- the benchmark or trace covers the deployed path if performance is part of the claim

## Notes

- This matrix is intentionally broader than any single module.
- If a row does not have a test yet, it should still have a documented
  failure mode.
- For streaming and policy logic, the absence of an edge-case test is usually a
  correctness bug, not a nice-to-have.
