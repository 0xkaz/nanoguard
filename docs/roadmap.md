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
- provider matrix expansion (see below)

### Provider matrix expansion (post-v1, opt-in)

Today nanoguard speaks OpenAI and Anthropic on the wire (with
`src/proxy/anthropic.rs` handling the Anthropic-client →
OpenAI-backend translation for tool calls and content blocks).
Google Gemini, AWS Bedrock, Cohere, Vertex, and the long tail of
provider formats are explicitly out of scope on the core path.

There is room to grow this if a deployment wants one binary
instead of a downstream LiteLLM hop, **provided four design lines
hold**:

1. **Pluggable provider clients, gated behind Cargo features.** The
   default release binary continues to ship `openai` + `anthropic` +
   `ollama` only. Additional providers (`gemini`, `bedrock`,
   `cohere`, etc.) live behind `--features <provider>` so the
   default binary stays small and the audit surface stays narrow.
   `CLAUDE.md > Performance Targets > Memory: < 10MB baseline`
   remains the bar the default build is measured against.
2. **OpenAI-shaped IR, not an N² translation matrix.** Provider
   adapters convert to/from a single internal representation
   (OpenAI's request / response shape, since it is already the
   lingua franca of OpenAI-compatible servers and the format
   `src/matcher`, `src/proxy/redact`, `src/guard/tool_gate`, and
   `src/guard/schema` all operate on). N+N adapters total; adding a
   provider is a single new file, not a new column.
3. **Guardrails stay first-class, the gateway stays second.** Every
   adapter must be audit-symmetric with the OpenAI path —
   `redactor.redact_text_in`, `Matchers::check_input`, tool gate,
   schema validation all run on the IR, so a new provider gains the
   guardrails for free. If an adapter needs to bypass any of those
   to make a provider work, the adapter is wrong, not the
   guardrails.
4. **The README continues to recommend LiteLLM downstream as the
   default deployment.** The Comparison table's phrasing —
   *"nanoguard is designed to sit in front of those tools as a
   dedicated policy boundary, not replace them"* — stays correct.
   Provider matrix expansion is for deployments that explicitly do
   not want a Python service in the path; it is not how new users
   are advised to scale up.

**Non-goals** for this work:

- Cost tracking, provider-aware retry, smart fallback routing,
  multi-region failover. Those are LiteLLM's territory; nanoguard
  does not replicate them.
- Image / audio / video modalities. Modality coverage is a separate
  axis from provider matrix and is its own future doc.
- Replacing the existing OpenAI/Anthropic adapters. The IR
  refactor is additive — the existing two paths become the
  reference implementations the new providers translate against.

The first plausible candidate is **Google Gemini**, both because
its wire format is well-documented and because it has the largest
"already on Google Cloud, prefer one binary" deployment audience.
See `docs/design/multi-backend-routing.md > Open questions` for
the design hook this will eventually plug into.

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
