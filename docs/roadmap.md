# Roadmap

## Shipped

- OpenAI-compatible chat proxy
- Anthropic-compatible chat proxy
- Aho-Corasick default keyword engine
- regex-backed structured pattern matching
- PII mask/reject/log path
- reversible redaction vault
- JSONL audit log
- SQLite budget tracking
- admin budget API
- buffered SSE output filtering
- ToolGate for tool-call evaluation
- schema validation module for structured responses and tool arguments

## Partial

- streaming usage accounting depends on backend metadata
- reversible redaction is opt-in and deployment-specific
- policy bundles and industry packs are still evolving
- tool-gate and schema coverage still need broader edge-case testing

## Proposed

- richer policy bundle format
- more deployment examples
- edge-specific packaging guidance
- stronger threat-model documentation
- additional observability/export targets
- signed audit export
- billing-grade budget rollups

## Notes

The main product direction is still the same:

- keep the proxy small
- keep the defaults local
- keep deterministic rules as the core
- add optional complexity only where the operational value is clear

## Working Agreement

When new behavior lands, add or update:

- a `docs/design/*.md` contract note if the behavior is user-visible
- a `docs/research/*.md` note if the behavior changes a tradeoff
- the README only after the code and tests are in place
