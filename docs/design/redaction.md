> **Status:** shipped (commit 82d65f3, 2026-05-10)

# PII Redaction

nanoguard can redact or reject PII before a request reaches the backend.
This is separate from keyword guardrails.

## Modes

The redaction system supports three actions:

- `mask` - replace the value with a placeholder before forwarding
- `reject` - block the request
- `log` - record a match and forward the request unchanged

The global default comes from `[input.pii].action`.
Per-entity overrides can be provided in `input.pii.entities`.

## Entity Rules

Entity patterns are regular expressions associated with entity names.
The built-in set covers common patterns such as:

- email addresses
- SSNs
- credit-card-shaped strings
- JWT-like tokens
- AWS access key style strings
- GitHub PAT style strings

Additional entity dictionaries can be loaded from `input.pii.dict_paths`.

## Placeholder Styles

Redaction placeholders are configurable:

- `bare` -> `[EMAIL]`
- `indexed` -> `[EMAIL_1]`, `[EMAIL_2]`
- `llm_guard` -> `[REDACTED_EMAIL_1]`

`reversible = true` promotes the style to an indexed placeholder format.
That is required so a per-request vault can map placeholders back to original
values after the LLM round-trip.

## Request Shapes

PII redaction walks request bodies that look like OpenAI or Anthropic chat
messages.

Supported shapes include:

- `messages[].content` as a string
- `messages[].content` as an array of text/image parts
- Anthropic text/block content shapes handled by the Anthropic proxy path

Only text-bearing fields are rewritten.
Images and non-text payloads are left unchanged.

## Reversible Flow

When reversible redaction is enabled:

1. The proxy creates a per-request vault.
2. Redaction stores placeholder -> original mappings in the vault.
3. The LLM sees placeholders, not raw PII.
4. The output path can deanonymize matching placeholders after the model
   returns.

This is opt-in.
It increases complexity and should be treated as a deployment-specific
feature, not the default privacy posture.

## Security Notes

Redaction is best-effort deterministic matching.
It does not claim to recover or sanitize arbitrary obfuscation.
It is useful for:

- obvious leakage prevention
- policy enforcement
- auditability

It is not a substitute for data minimization, secret management, or access
control.

