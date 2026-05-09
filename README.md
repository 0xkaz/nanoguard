# nanoguard

[![CI](https://github.com/0xkaz/nanoguard/actions/workflows/ci.yml/badge.svg)](https://github.com/0xkaz/nanoguard/actions/workflows/ci.yml)
[![Release](https://github.com/0xkaz/nanoguard/actions/workflows/release.yml/badge.svg)](https://github.com/0xkaz/nanoguard/actions/workflows/release.yml)
[![ghcr.io](https://img.shields.io/badge/ghcr.io-nanoguard-blue)](https://github.com/0xkaz/nanoguard/pkgs/container/nanoguard)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

**LLM guardrails proxy. Rust. Single binary. No GPU. No cloud. ~7 µs to block.**

nanoguard sits between your application and any OpenAI-compatible LLM backend,
filtering prompts and responses in microseconds — entirely in-process, entirely offline.

```
[Your App / Robot / Edge Device / Open WebUI]
        ↓  POST /v1/chat/completions
  ┌──────────────────────────────────────┐
  │              nanoguard               │
  │  Input Guardrails  (~7 µs)           │  ← keyword / PII regex / injection patterns
  │  Backend Router                      │  ← Ollama / OpenAI / Anthropic / any
  │  Output Guardrails (~4 µs)           │  ← sensitive word masking, streaming
  │  Audit Log                           │  ← JSONL, SHA-256 hash-only mode
  └──────────────────────────────────────┘
        ↓
  [LLM Backend]
```

---

## The problem

Cloud guardrail APIs (AWS Bedrock Guardrails, Azure AI Content Safety, Google Cloud) work well when you have a reliable internet connection and can tolerate the added cloud roundtrip latency per request. For a web chatbot, that is fine.

For everything else, it is a hard architectural mismatch:

- **Robotics and autonomous systems** require sub-100ms decision cycles. A 500ms cloud roundtrip for content filtering is longer than the control loop itself.
- **Industrial IoT and factory floors** often operate on isolated networks with no outbound internet access by design.
- **Medical and government systems** cannot route patient or classified data through external APIs for policy reasons, regardless of latency.
- **Edge devices** — from warehouse robots to in-vehicle systems — face intermittent connectivity as a physical reality, not an edge case.

For latency-sensitive or offline applications, safety filtering has to run locally — cloud guardrails are a single point of failure for systems that cannot stop and wait.

nanoguard is a deterministic, offline-capable guardrails layer that runs anywhere a binary can run.

---

## Design decisions

### Why Rust

Rust gives deterministic latency without a garbage collector and memory safety without a VM. The release binary is a single statically-linked executable under 10MB — no interpreter, no package manager, nothing to update at deploy time.

Go would have been a reasonable alternative: similar deployment story, faster iteration. The tradeoff is GC pause predictability and the fact that a security boundary benefits from compile-time memory safety guarantees. For a proxy that sits in the path of every LLM request, the latency tail and the attack surface both matter.

### Why deterministic rules, not a second LLM

LLM-based content filtering is appealing — it handles nuance that keyword lists miss. The cost: 100ms–2s added latency, a dependency on model availability, and verdicts that are probabilistic rather than auditable. "The guard model said allow" is not a compliance record.

For the majority of enterprise policy requirements — prompt injection patterns, PII categories, off-topic domains, banned keywords — a well-curated rule set is sufficient, deterministic, and auditable line by line. nanoguard uses that approach as the default. LLM-based filtering is planned as an opt-in feature.

### Why iword-rs as the filter core

Most keyword filters are O(N × M): one scan per pattern. [iword-rs](https://github.com/0xkaz/iword-rs) uses a sliding-window double rolling hash with binary search lookup — a single O(N) pass where scan time scales with text length, not pattern count. Preprocessing is lightweight (hash table, no automaton construction), which keeps startup cost and memory low — well suited to the typical guardrails use case of tens to a few hundred patterns.

A 2,700-character prompt scans in ~950 µs. A blocked request exits in ~7 µs. The same engine handles keyword blocks, PII alerts, and regex patterns through one interface.

---

## Comparison

| | nanoguard | AWS Bedrock Guardrails | Azure AI Content Safety | LLM Guard | LiteLLM |
|---|---|---|---|---|---|
| Deployment | Single binary | Cloud API | Cloud API / Embedded† | Python lib | Python app |
| Offline | ✅ | ❌ | ❌ / △† | ✅ | ✅ |
| GPU-free | ✅ | N/A | N/A | △ | ✅ |
| Filter latency | ~7–50 µs* | cloud roundtrip | cloud roundtrip | model-dependent | not applicable |
| External guardrail call | ❌ never | ✅ required | ✅ required | ❌ never | depends |
| Audit log | ✅ JSONL | ✅ CloudWatch | ✅ Azure Monitor | △ | ✅ (Enterprise) |

\* Measured locally; see [Performance](#performance) section.  
† Azure Embedded Content Safety requires separate approval and is not generally available.

LiteLLM and multi-provider gateways solve a different problem (routing, observability across providers). nanoguard is designed to sit in front of those tools as a dedicated security boundary, not replace them.

---

## Quick start

### Docker (recommended)

```bash
# Architecture auto-detected: arm64 / amd64
docker run -p 8080:8080 \
  -e BACKEND_ENDPOINT=http://host.docker.internal:11434 \
  ghcr.io/0xkaz/nanoguard:latest
```

With a config file:

```bash
docker run -p 8080:8080 \
  -v $(pwd)/nanoguard.toml:/app/nanoguard.toml:ro \
  ghcr.io/0xkaz/nanoguard:latest
```

> Image: [ghcr.io/0xkaz/nanoguard](https://github.com/0xkaz/nanoguard/pkgs/container/nanoguard) — `linux/amd64` and `linux/arm64`

### With Ollama (build from source)

```bash
git clone https://github.com/0xkaz/nanoguard
cd nanoguard
make run   # pulls qwen3:0.6b, starts nanoguard on :8080
```

```bash
# Drop-in for any OpenAI client
curl http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"qwen3:0.6b","messages":[{"role":"user","content":"Hello!"}]}'
```

### With OpenAI

```bash
export OPENAI_API_KEY=sk-...
export BACKEND_PROVIDER=openai
export BACKEND_ENDPOINT=https://api.openai.com
export BACKEND_MODEL=gpt-4o-mini
make run
```

---

## Endpoints

| Endpoint | Description |
|----------|-------------|
| `POST /v1/chat/completions` | OpenAI-compatible chat |
| `POST /v1/messages` | Anthropic-compatible chat (text only; tool_use / vision / streaming not yet supported) |
| `GET /v1/models` | Proxy to backend model list |
| `GET /health` | Health check |
| `GET /v1/admin/budget/:api_key` | Token usage + limit (requires admin key) |
| `PUT /v1/admin/budget/:api_key` | Set limit `{"limit": 100000}` |
| `DELETE /v1/admin/budget/:api_key/reset` | Reset usage counter |

---

## Guardrails

### Input (before sending to LLM)

Single O(N) pass via [iword-rs](https://github.com/0xkaz/iword-rs) (sliding-window rolling hash). Blocked requests never reach the backend.

| Result | Action |
|--------|--------|
| **Blocked** | Rejected immediately, LLM never called |
| **Alert** | Logged, forwarded |
| **Flagged** | Logged, forwarded |

Default blocked patterns (fully configurable):

```
ignore previous instructions · disregard your instructions · jailbreak · dan mode · you are now
```

### Output (before returning to client)

Responses are filtered before reaching your app. Sensitive words are replaced with `***`.
Streaming responses are filtered chunk-by-chunk on the SSE `delta.content` field.

### Audit log

When enabled, every request writes one JSONL line:

```json
{
  "request_id": "000000000000000018adfc678fe51118",
  "timestamp": "2026-05-09T19:29:09.607304+00:00",
  "api_key": "default",
  "model": "qwen3:0.6b",
  "prompt_hash": "d4c067494508331c38975ab3356c6f213c1f571969d3d2c1ad8d1c45d1bc20db",
  "verdict": "block",
  "matched_rule": "jailbreak",
  "latency_us": 102
}
```

`hash_only = true` (default): SHA-256 hash of the prompt only — no raw text stored.
Useful for compliance environments where retaining user input is restricted.

---

## Configuration

`nanoguard.toml`:

```toml
[nanoguard]
listen = "0.0.0.0:8080"
log_level = "info"

[backend]
provider = "ollama"
endpoint = "http://localhost:11434"
# api_key = "sk-..."
# model = "llama3.2"

[input]
enabled = true

[input.keyword]
dict_paths = []  # additional .txt word list files
inline_block = ["ignore previous instructions", "jailbreak"]
inline_alert = ["password", "api_key"]
inline_flag  = ["bitcoin", "crypto"]

[output]
enabled = true

[budget]
enabled = false
db_path = "nanoguard.db"
# admin_api_key = "your-secret-key"

[audit]
enabled = false
path = "nanoguard-audit.jsonl"
hash_only = true
```

### Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `NANOGUARD_CONFIG` | `nanoguard.toml` | Config file path |
| `BACKEND_PROVIDER` | `ollama` | `ollama` / `openai` / any OpenAI-compatible |
| `BACKEND_ENDPOINT` | `http://localhost:11434` | Backend base URL |
| `BACKEND_API_KEY` | — | API key (also reads `OPENAI_API_KEY`) |
| `BACKEND_MODEL` | — | Default model name |
| `RUST_LOG` | `info` | Log level |
| `ADMIN_API_KEY` | — | Enables `/v1/admin/budget/*` endpoints |

---

## Build

```bash
make          # release binary → target/release/nanoguard
make dev      # debug build + run
make test     # cargo test
make check    # clippy + fmt
make bench    # criterion benchmarks → target/criterion/
```

Requires: Rust 1.75+

---

## Dictionary files

[iword-rs](https://github.com/0xkaz/iword-rs) format — tab-separated:

```
# word          key   weight
confidential    0            # BLOCK
internal only   0     5.0   # BLOCK, weighted
project_xyz     1            # ALERT
```

```toml
[input.keyword]
dict_paths = ["dicts/company.txt", "dicts/prompt_injection.txt"]
```

---

## Performance

Measured on Apple M-series, single core, release build, warm cache.
**These are guardrail-only costs — HTTP overhead and LLM round-trip are not included.**

| Operation | Time |
|---|---|
| Input check — clean | ~10 µs |
| Input check — blocked (early exit) | ~7 µs |
| Output filter — no match | ~4 µs |
| Output filter — mask SSN + credit card | ~7 µs |

Input scanning is O(N) in prompt length. A 2,700-character prompt: ~950 µs.

---

## Security

nanoguard minimizes external dependencies by design.
All filtering runs in-process with no network calls and no dynamic code at runtime.
The release binary is statically linked — what you audit is what runs.

### Limitations

nanoguard is a **policy enforcement layer**, not an adversarial-resistant security boundary.
Rule-based keyword filtering can be bypassed by a motivated attacker using encoding tricks,
paraphrasing, or obfuscation. It is designed for:

- Compliance and audit trails
- Preventing accidental misuse (prompt injection from untrusted content)
- Enforcing organizational policy on LLM usage

It is **not** designed to defeat adversarial users who are actively trying to circumvent the filter.
For threat models that include motivated attackers, combine nanoguard with additional controls.

Streaming output filtering is currently chunk-local. Matches split across SSE chunk boundaries may
require buffered scanning in a future release.

---

## Credits

Powered by [iword-rs](https://github.com/0xkaz/iword-rs) — Pure Rust O(N) keyword search.
