> **Status:** shipped (v0.7.0, 2026-05-10)

# Policy Engine

A policy bundle is a versioned YAML file that declares rules with stable
identifiers, a category, a severity, and an action. Loading the bundle at
startup is opt-in; rules from the bundle are merged into the existing
inline keyword and PII redactor configuration, and matches against those
rules surface their metadata in the audit log.

## Why It Exists

Inline TOML lists answer "is this string blocked?". They do not answer:

- which rule blocked it
- which category did it belong to
- how severe was the match
- which compliance regime did the rule trace to

A SIEM, a compliance reviewer, and a customer-support reply all need
those answers, and a string match alone cannot give them. The policy
engine adds a thin metadata layer on top of the existing matchers so
those questions can be answered without changing the matcher hot path.

## Scope of v1

In scope:

- keyword rules with `block` / `alert` / `flag` actions
- PII regex rules with `redact` action and a `placeholder` entity name
- per-rule `id`, `category`, `severity`, `compliance` metadata
- audit log enrichment with the matched rule's metadata
- a single bundle loaded from disk at startup

Out of scope (deliberately, for v1):

- hot reload
- signed bundles
- multiple stacked bundles
- spotlighting / tool gate / schema rule types
- per-tenant overrides

## Configuration

`nanoguard.toml`:

```toml
[policies]
bundle_path = "policies/default.yaml"
```

Bundle file:

```yaml
version: 1
metadata:
  name: nanoguard default
  updated: "2026-05-10"

rules:
  - id: PI-001
    category: prompt_injection
    severity: high
    pattern: ignore previous instructions
    action: block

  - id: PII-001
    category: pii
    severity: medium
    pattern: '/[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}/'
    action: redact
    placeholder: EMAIL
    compliance: ["GDPR", "HIPAA"]
```

A pattern wrapped in `/.../` is treated as a regex; otherwise it is a
literal multi-word phrase that goes through the same case-insensitive
normalize step as inline keyword rules.

## Audit Output

When a request matches a policy rule, the audit entry gains four
optional fields. They are omitted entirely when no policy is loaded,
preserving backward-compatible shape:

```json
{
  "request_id": "...",
  "verdict": "block",
  "matched_rule": "ignore previous instructions",
  "rule_id": "PI-001",
  "category": "prompt_injection",
  "severity": "high",
  "compliance": [],
  "latency_us": 102
}
```

Shadow-mode entries strip the `shadow_block:` prefix from the matched
rule before the policy index is consulted, so demoted rules still
surface their metadata.

## How It Plugs In

At startup:

1. `Config::from_env_or_default` parses `[policies] bundle_path`.
2. If a path is set, `Policy::from_path` loads and validates the YAML.
3. Literal-pattern rules are appended to `KeywordConfig.inline_block` /
   `inline_alert` / `inline_flag` based on `action`.
4. A `PolicyRuleIndex` is built and stored on `AppState.policy`.
5. The audit writer consults the index when it has a `matched_rule` and
   enriches the entry with `rule_id` / `category` / `severity` /
   `compliance`.

The matcher and redactor are untouched on the hot path; the policy index
is only consulted at audit time.

## Limitations

- A literal pattern in a policy file does not yet feed the redactor. To
  redact PII from a custom regex, declare a regex rule (`/.../`) and a
  `placeholder` and the redactor will wire it up at startup.
- `redact` / `reject` / `log` actions on a *literal* pattern are
  currently no-ops. Use a regex pattern when an entity-style action is
  needed.
- A duplicate rule id across different bundles is rejected at load time
  rather than allowed and shadowed.

These are intentional gaps for v1 and tracked in `_POLICY_ENGINE.md`.
