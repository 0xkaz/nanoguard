> **Status:** shipped (v0.5.0, 2026-05-10)

# Schema Validation vs Guardrails

Schema validation and guardrails are complementary, not interchangeable.

## Schema Validation

Schema validation checks whether structured data matches an expected shape.
It is good at answering:

- do required fields exist?
- are types correct?
- do strings match expected patterns?
- are unexpected fields present?

In nanoguard, schema validation is most useful for:

- LLM tool call arguments
- structured JSON responses
- API payloads that must stay machine-readable

## Guardrails

Guardrails check policy.
They are good at answering:

- should this prompt be blocked?
- should this text be redacted?
- should this tool call be denied?
- should this request be logged or shadowed?

Guardrails are policy enforcement.
They are not just structure enforcement.

## Why Both Matter

An LLM can produce:

- a perfectly valid JSON object with unsafe content
- an invalid JSON object that still looks superficially correct
- a structurally valid tool call that should never be executed

Schema validation catches the second case.
Guardrails catch the first and third cases.

## Practical Rule

Use schema validation when the application needs machine-readable shape.
Use guardrails when the application needs policy.
Use both when the model output can become a side effect.

## In nanoguard

The current architecture uses this rough order:

1. normalize and scan input
2. redact or reject PII
3. forward the request
4. filter output
5. inspect tool calls and structured outputs

That ordering keeps policy checks close to the proxy boundary and leaves
schema checks where structured data becomes executable or ingestible.

## Takeaway

If the question is "is this data shaped correctly?", use schema validation.
If the question is "should this data be allowed to exist here at all?",
use guardrails.

