> **Status:** shipped (v0.5.0, 2026-05-10)

# Tool Gate Comparison

ToolGate is the part of nanoguard that inspects model-emitted tool calls
before the application executes them.

This note compares ToolGate with the other policy layers in the project.

## What ToolGate Solves

ToolGate sits between a model and the application-side execution of tools.
It is meant to stop a model from turning a textual prompt into an unsafe
external action.

The basic checks are:

- tool allow/deny lists
- JSON Schema validation for tool arguments
- PII / secret scanning of tool arguments
- optional sanitization of tool arguments

## Why ToolGate Is Not the Same as Schema Validation

Schema validation answers:

- "are these arguments shaped correctly?"

ToolGate answers:

- "should this tool be allowed at all?"
- "should these arguments be redacted?"
- "should the tool call be denied because it leaks secrets?"

Schema validation is structural.
ToolGate is structural plus policy.

## Why ToolGate Is Not the Same as Keyword Guardrails

Keyword guardrails act on user input before the model runs.
ToolGate acts on model-emitted tool calls after the model has already
decided to act.

That difference matters because tool misuse can happen even when the input
was perfectly benign.

## Why ToolGate Is Different From an Agent Router

ToolGate does not choose tools.
It does not plan tasks.
It does not call the network.
It only evaluates a tool call that already exists.

That keeps the responsibility narrow and testable.

## Operational Value

ToolGate is useful because it gives you a last deterministic checkpoint before
the application executes a side effect.

That is especially valuable for:

- email
- ticketing
- payments
- admin APIs
- shell-like operations

## Comparison Summary

| Layer | Input | Decision | Typical Output |
|---|---|---|---|
| Keyword guardrails | user text | block / alert / flag | request denied or logged |
| Schema validation | structured JSON | pass / fail | valid or invalid payload |
| ToolGate | model-emitted tool call | allow / deny / sanitize | executable, denied, or redacted tool args |

ToolGate becomes most valuable when the model can cause a real-world action.

