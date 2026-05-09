# nanoguard

[![CI](https://github.com/0xkaz/nanoguard/actions/workflows/ci.yml/badge.svg)](https://github.com/0xkaz/nanoguard/actions/workflows/ci.yml)
[![Release](https://github.com/0xkaz/nanoguard/actions/workflows/release.yml/badge.svg)](https://github.com/0xkaz/nanoguard/actions/workflows/release.yml)
[![ghcr.io](https://img.shields.io/badge/ghcr.io-nanoguard-blue)](https://github.com/0xkaz/nanoguard/pkgs/container/nanoguard)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

**Nano-fast. CPU-only. Offline-first. LLM Guardrails Proxy.**

nanoguard is a Rust-native LLM guardrails proxy.
It sits between your application and any OpenAI-compatible LLM backend,
filtering prompts and responses in microseconds — no GPU, no cloud, no Python.

```
[Your App / Open WebUI / Telegram Bot]
        ↓  POST /v1/chat/completions
  ┌─────────────────────────────┐
  │         nanoguard           │
  │  Input Guardrails  (μs)     │  ← keyword filter, PII detection
  │  Backend Router             │  ← Ollama / OpenAI / Anthropic
  │  Output Guardrails (μs)     │  ← sensitive word masking
  │  Audit Log                  │  ← JSONL, privacy-preserving hash
  └─────────────────────────────┘
        ↓
  [LLM Backend]
```

---

## Why nanoguard exists

### The gap this fills

Existing guardrails tools (LLM Guard, NeMo Guardrails) are research-oriented:
they rely on ML models or GPU-accelerated scanners and carry a 300MB+ footprint.
They are not designed to run as a transparent proxy in a production request path.

Multi-provider LLM gateways (LiteLLM, Portkey, Helicone) solve a different
problem — routing and observability across many providers — and are excellent at
that. nanoguard is designed to sit **in front of** those tools, not replace them,
as a dedicated security boundary.

The gap: there is no lightweight, single-binary, offline-capable tool focused
purely on enforcing content policy at the LLM traffic boundary, fast enough to
be invisible in the request path.

### What nanoguard is (and is not)

nanoguard is an **AI Security Gateway** — a thin, auditable layer you place
in front of any LLM backend to enforce policy at the boundary:

- What content is allowed into the LLM
- What content is allowed out of the LLM
- Who sent what, when, and what verdict was applied

It is designed for environments where operational simplicity is a hard constraint:
air-gapped factories, medical networks, edge devices, or any team that wants a
sub-millisecond security layer with a 10MB memory budget and a single binary to
audit and deploy.

---

## Design decisions

### Why Rust

Memory safety without garbage collection pauses matters when you are on the hot
path of every LLM request. Rust gives deterministic latency, a sub-10MB baseline
RSS, and no runtime — the release artifact is a single statically-linked binary.
There is no interpreter, no package manager, and no dynamic loader to compromise
at deploy time.

Rust's ownership model also eliminates entire classes of bugs (use-after-free,
data races) that are common in C/C++ network daemons, without the GC overhead
of Go. For a security proxy, that tradeoff is worth the steeper learning curve.

### Why iword-rs as the filter core

Most keyword filters are O(N × M) — they scan the input once per pattern.
[iword-rs](https://github.com/0xkaz/iword-rs) uses an Aho-Corasick automaton:
a single O(N) pass over the input, regardless of how many patterns are loaded.
Loading 10,000 rules costs the same scan time as loading 10.

This matters for two reasons:

1. **Latency:** input filtering completes in microseconds, not milliseconds.
   A 2,700-character prompt scans in ~950 µs on Apple M-series.
2. **Design consistency:** the same engine handles keyword blocks, PII alerts,
   and regex patterns through a unified interface. There is no "fast path" and
   "slow path" — all rules go through one scanner.

### Why no LLM-based filtering

Using a second LLM to judge the first LLM's traffic introduces:

- A second latency budget (100ms–2s per call)
- A second failure mode (what if the guard LLM is compromised or hallucinating?)
- A second cost center
- An internet dependency for cloud-hosted guard models

nanoguard's filter is deterministic and offline. A rule that blocks "SSN" blocks
it in 7 µs, every time, with no network call and no model inference. For the
majority of enterprise policy requirements — prompt injection patterns, PII
categories, off-topic content — deterministic rules are sufficient and auditable
in a way that LLM-based verdicts are not.

LLM-based filtering is supported as an opt-in future feature, never the default.

### Offline and edge first

Every feature in nanoguard works without internet access. This is a hard
constraint, not a nice-to-have. The target environments — hospital networks,
factory floors, government intranets, air-gapped Kubernetes clusters — often
cannot make outbound HTTP calls. Cloud-only features are prohibited in the core.

---

## Comparison

| | nanoguard | LLM Guard | NeMo | LiteLLM |
|---|---|---|---|---|
| Language | Rust | Python | Python | Python |
| Role | Security Gateway | Guardrails lib | Guardrails lib | LLM Gateway |
| Proxy | ✅ | ❌ | ❌ | ✅ |
| Offline | ✅ | ✅ | △ | ✅ |
| GPU-free | ✅ | △ | ❌ | ✅ |
| Single binary | ✅ | ❌ | ❌ | ❌ |
| Memory | ~10MB | ~300MB+ | heavy | ~500MB+ |
| Filter latency | ~7–50 µs | 10ms–2s | 10ms+ | N/A |
| Audit log | ✅ (JSONL, hash-only mode) | △ | △ | ✅ (Enterprise) |

These tools solve different problems. nanoguard pairs well with LiteLLM:
nanoguard handles the security boundary, LiteLLM handles multi-provider routing.

---

## Quick start

### Docker (recommended)

```bash
# Architecture is auto-detected (arm64 / amd64)
docker run -p 8080:8080 \
  -e BACKEND_ENDPOINT=http://host.docker.internal:11434 \
  ghcr.io/0xkaz/nanoguard:latest
```

With a custom config:

```bash
docker run -p 8080:8080 \
  -v $(pwd)/nanoguard.toml:/app/nanoguard.toml:ro \
  ghcr.io/0xkaz/nanoguard:latest
```

> Image published to [ghcr.io/0xkaz/nanoguard](https://github.com/0xkaz/nanoguard/pkgs/container/nanoguard) on every tagged release.
> Supports `linux/amd64` and `linux/arm64` (Apple Silicon / AWS Graviton).

### With Ollama (build from source)

```bash
git clone https://github.com/0xkaz/nanoguard
cd nanoguard
make run   # pulls qwen3:0.6b, runs nanoguard on :8080
```

```bash
# Use it exactly like the OpenAI API
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
| `POST /v1/chat/completions` | OpenAI-compatible chat — proxied through guardrails |
| `POST /v1/messages` | Anthropic-compatible chat — proxied through guardrails |
| `GET /v1/models` | List models from backend |
| `GET /health` | Health check |
| `GET /v1/admin/budget/:api_key` | Get token usage and limit (requires admin key) |
| `PUT /v1/admin/budget/:api_key` | Set token limit `{"limit": 100000}` (requires admin key) |
| `DELETE /v1/admin/budget/:api_key/reset` | Reset usage counter (requires admin key) |

---

## Guardrails

### Input (before sending to LLM)

Requests are scanned using [iword-rs](https://github.com/0xkaz/iword-rs) — a single-pass O(N) Aho-Corasick engine.

| Result | Action |
|--------|--------|
| **Blocked** | Request rejected immediately, LLM never called |
| **Alert** | Logged, request forwarded |
| **Flagged** | Logged, request forwarded |

Default blocked patterns (configurable):

```
ignore previous instructions
disregard your instructions
jailbreak
dan mode
you are now
```

### Output (before returning to client)

LLM responses are scanned and filtered before reaching your application.
Sensitive words (`ssn`, `credit card`, etc.) are replaced with `***`.
Streaming responses are filtered chunk-by-chunk.

### Audit log

Every request produces a structured JSONL entry:

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

`hash_only = true` (default) logs a SHA-256 hash of the prompt instead of the
raw text — useful for compliance environments where storing user input is
restricted.

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
dict_paths = []  # load additional .txt word list files
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
hash_only = true   # SHA-256 hash of prompt only (privacy-preserving)
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

Load custom word lists in [iword-rs](https://github.com/0xkaz/iword-rs) format:

```
# words.txt (tab-separated: word  key  weight)
confidential      0          # BLOCK
internal use only 0   5.0   # BLOCK, weight 5.0
project_codename  1          # ALERT
```

```toml
[input.keyword]
dict_paths = ["dicts/company.txt", "dicts/prompt_injection.txt"]
```

---

## Performance

Measured on Apple M-series (single core, release build).
These are guardrail costs only — not including network round-trip to the LLM.

| Operation | Time |
|---|---|
| Input check — clean (no match) | ~10 µs |
| Input check — blocked (early exit) | ~7 µs |
| Input check — multiline normalization + block | ~7 µs |
| Output filter — no sensitive words | ~4 µs |
| Output filter — mask SSN + credit card | ~7 µs |

Input scanning is O(N) in prompt length. A 2,700-character prompt costs ~950 µs.

---

## Use cases

- **Local LLM protection** — wrap Ollama with guardrails for team use
- **Air-gapped environments** — factory, medical, government networks with no cloud
- **LiteLLM front layer** — add a security boundary in front of an existing LLM gateway
- **Bot safety** — protect Telegram / Slack bots from prompt injection
- **Cost control** — block off-topic or abusive prompts before they reach paid APIs

---

## Security policy

nanoguard is written in Rust and intentionally minimizes external dependencies.
All filtering runs in-process — no network calls, no runtime scripts, no package
manager at deploy time. The release artifact is a single statically-linked binary.
What you audit is what runs.

---

## Credits

Powered by [iword-rs](https://github.com/0xkaz/iword-rs) — Pure Rust O(N) keyword search.
