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
- streaming output filtering

## Partial

- streaming usage accounting depends on backend metadata
- reversible redaction is opt-in and deployment-specific
- policy bundles and industry packs exist but are still evolving

## Proposed

- richer policy bundle format
- more deployment examples
- edge-specific packaging guidance
- stronger threat-model documentation
- additional observability/export targets

## Notes

The main product direction is still the same:

- keep the proxy small
- keep the defaults local
- keep deterministic rules as the core
- add optional complexity only where the operational value is clear

