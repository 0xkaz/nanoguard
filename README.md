# nanoguard

[![CI](https://github.com/0xkaz/nanoguard/actions/workflows/ci.yml/badge.svg)](https://github.com/0xkaz/nanoguard/actions/workflows/ci.yml)
[![Release](https://github.com/0xkaz/nanoguard/actions/workflows/release.yml/badge.svg)](https://github.com/0xkaz/nanoguard/actions/workflows/release.yml)
[![ghcr.io](https://img.shields.io/badge/ghcr.io-nanoguard-blue)](https://github.com/0xkaz/nanoguard/pkgs/container/nanoguard)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

**LLM guardrails proxy. Rust. Single binary. No GPU. No cloud guardrail call. Sub-microsecond literal blocks.**

nanoguard sits between your application and any OpenAI-compatible LLM backend,
filtering prompts and responses in-process, without an external guardrail API.

```
[Your App / Robot / Edge Device / Open WebUI]
        ↓  POST /v1/chat/completions
  ┌──────────────────────────────────────┐
  │              nanoguard               │
  │  Input Guardrails  (<1 µs literal)   │  ← keyword / PII regex / injection patterns
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

- **Robotics and autonomous systems** often expose natural-language operator commands, task planning, and fleet-control interfaces where cloud filtering can add unacceptable latency or fail under poor connectivity.
- **Industrial IoT and factory floors** often operate on isolated networks with no outbound internet access by design.
- **Medical and government systems** cannot route patient or classified data through external APIs for policy reasons, regardless of latency.
- **Edge devices** — from warehouse robots to in-vehicle systems — face intermittent connectivity as a physical reality, not an edge case.

For latency-sensitive or offline applications, safety filtering has to run locally — cloud guardrails are a single point of failure for systems that cannot stop and wait.

nanoguard is a deterministic, offline-capable guardrails layer that runs anywhere a binary can run.

---

## Design decisions

### Why Rust

Rust gives deterministic latency without a garbage collector and memory safety without a VM. The release artifact is a compact single binary — no interpreter, no package manager, nothing to update at deploy time.

Go would have been a reasonable alternative: similar deployment story, faster iteration. The tradeoff is GC pause predictability and the fact that a security boundary benefits from compile-time memory safety guarantees. For a proxy that sits in the path of every LLM request, the latency tail and the attack surface both matter.

### Why deterministic rules, not a second LLM

LLM-based content filtering is appealing — it handles nuance that keyword lists miss. The cost: significant added latency per call, a dependency on model availability, and verdicts that are probabilistic rather than auditable. "The guard model said allow" is not a compliance record.

For the majority of enterprise policy requirements — prompt injection patterns, PII categories, off-topic domains, banned keywords — a well-curated rule set is sufficient, deterministic, and auditable line by line. nanoguard uses that approach as the default. LLM-based filtering is planned as an opt-in feature.

### Why Aho-Corasick + regex rules

Most naive keyword filters are O(N × M): one scan per pattern. nanoguard uses [aho-corasick](https://docs.rs/aho-corasick/) for literal rules, so many prompt-injection phrases, PII terms, and policy keywords are matched in a single O(N) pass. Regex rules are compiled separately for patterns that need structure, such as email addresses, SSNs, and API-token shapes.

The default engine is `aho-corasick`; the older `iword-rs` engine remains available via config for comparison and compatibility.

---

## Comparison

| | nanoguard | AWS Bedrock Guardrails | Azure AI Content Safety | LLM Guard | LiteLLM |
|---|---|---|---|---|---|
| Deployment | Single binary | Cloud API | Cloud API / Embedded† | Python lib | Python app |
| Offline | ✅ | ❌ | ❌ / △† | ✅ | ✅ |
| GPU-free | ✅ | N/A | N/A | depends on scanner | ✅ |
| Filter latency | ~0.2–65 µs* | cloud roundtrip | cloud roundtrip | model-dependent | not applicable |
| External guardrail call | ❌ never | ✅ required | ✅ required | ❌ never | depends |
| Audit log | ✅ JSONL | ✅ CloudWatch | ✅ Azure Monitor | △ | ✅ (Enterprise) |

\* Measured locally; see [Performance](#performance) section.  
† Azure Embedded Content Safety requires separate approval and is not generally available.

LiteLLM and multi-provider gateways solve a different problem (routing, observability across providers). nanoguard is designed to sit in front of those tools as a dedicated policy boundary, not replace them.

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
| `POST /v1/messages` | Anthropic-compatible chat (text + `tool_use`; vision and streaming not yet supported) |
| `GET /v1/models` | Proxy to backend model list |
| `GET /health` | Health check |
| `GET /v1/admin/budget/:api_key` | Token usage + limit (requires admin key) |
| `PUT /v1/admin/budget/:api_key` | Set limit `{"limit": 100000}` |
| `DELETE /v1/admin/budget/:api_key/reset` | Reset usage counter |

---

## Guardrails

### Input (before sending to LLM)

Literal rules are scanned with Aho-Corasick; regex rules are compiled once at startup. Blocked requests never reach the backend.

| Result | Action |
|--------|--------|
| **Blocked** | Rejected immediately, LLM never called |
| **Alert** | Logged, forwarded |
| **Flagged** | Logged, forwarded |

Default blocked patterns (fully configurable):

```
ignore previous instructions · disregard your instructions · jailbreak · dan mode · you are now
```

#### PII redaction before forwarding

When `input.pii.enabled = true`, requests are scanned with a configurable set of named entity patterns. The default set covers email, US SSN, credit cards, AWS access keys, GitHub PATs, OpenAI/Anthropic/Stripe/Google API keys, JWTs, Slack tokens, and a generic high-entropy token catch-all. Add your own via `input.pii.dict_paths`.

| Action | Behavior |
|---|---|
| `mask` (default) | Replace each match in the request body with a `[<ENTITY_NAME>]` placeholder (e.g. `[EMAIL]`, `[AWS_ACCESS_KEY_ID]`, `[JWT]`) **before forwarding to the LLM**. The LLM receives the redacted prompt; the original is never sent. |
| `reject` | Block the request entirely if any pattern matches. |
| `log` | Forward unchanged but record an ALERT log line. |

Both `/v1/chat/completions` (string content and `parts[].text` arrays) and `/v1/messages` (Anthropic `Text` and `Blocks` shapes) are redacted before the request leaves the process.

User dictionaries follow the standard format with the entity name in the second column:

```
# dicts/my-org-pii.txt
/\bSEC-\d{6}\b/	INTERNAL_TICKET
/\bEMP-[A-Z0-9]{8}\b/	EMPLOYEE_ID
```

```toml
[input.pii]
enabled = true
action  = "mask"
dict_paths = ["dicts/my-org-pii.txt"]
placeholder_style = "bare"   # "bare" | "indexed" | "llm_guard"
```

`placeholder_style` controls how matches are formatted. The default `bare` style emits `[EMAIL]`, which is fastest but loses the distinction between different values. `indexed` emits `[EMAIL_1]`, `[EMAIL_2]` so the LLM can tell apart two distinct emails in the same prompt — this is also a stepping stone toward Vault-backed deanonymization. `llm_guard` emits `[REDACTED_EMAIL_1]` for wire-compatibility with prompts that follow the LLM Guard convention.

#### Per-entity action overrides

The global `action` setting applies to every entity by default, but you can override it per entity. `reject` always wins: if any reject-class entity matches, the request is blocked before any masking runs.

```toml
[input.pii]
action = "mask"   # default for entities not listed below

[input.pii.entities]
AWS_ACCESS_KEY_ID = "reject"
ANTHROPIC_KEY     = "reject"
GITHUB_PAT        = "reject"
JWT               = "reject"
EMAIL             = "mask"
PHONE             = "log"
```

Entities not present in the redactor (e.g. typos) are silently ignored. Entity names are case-sensitive and match what the redactor emits in placeholders.

#### Reversible redaction (Vault round-trip)

When `reversible = true`, mask-class matches are recorded in a per-request Vault and restored from the LLM's response. The model never sees the originals; the client sees the original prompt context preserved in the answer.

```toml
[input.pii]
enabled = true
action  = "mask"
reversible = true
deanonymize_strategy = "exact"   # "exact" (default) | "case_insensitive"
```

Round-trip example:

```
client → nanoguard:  "My email is alice@example.com"
nanoguard → LLM:     "My email is [EMAIL_1]"
LLM → nanoguard:     "You said: My email is [EMAIL_1]"
nanoguard → client:  "You said: My email is alice@example.com"
```

Vault scope is **per request** — placeholders set in one request are never visible to another. Session-scoped vaults (so turn 2 sees turn 1's `[EMAIL_1]`) are planned for a future opt-in feature.

Streaming responses are deanonymized through a state machine that buffers from `[` to `]`, so a placeholder split across SSE chunks is restored correctly.

`deanonymize_strategy = "fuzzy"` and `"combined"` are reserved names that currently fall back to `exact` with a warning.

#### Obfuscation handling

Input is normalized before scanning. Two transforms are on by default — they are essentially free for ASCII input (NFKC is skipped via a fast path) and only catch attacks that would otherwise slip through:

- **NFKC Unicode normalization** — full-width letters such as `ｊａｉｌｂｒｅａｋ` fold to ASCII before matching.
- **Zero-width character stripping** — `j​ailb​reak` becomes `jailbreak`.

Two more are opt-in because they can produce false positives on legitimate text:

- **`separators = true`** — collapses runs of single letters joined by `-` `.` `_` `*` `~` (e.g. `j-a-i-l-b-r-e-a-k`). A length-≥4 run threshold preserves common dashed words like `co-op` and `e-mail`.
- **`leet = true`** — folds digits and symbols to letters (`j41lbr34k` → `jailbreak`). Disabled by default because it affects legitimate text such as `3D printer` or `S3 bucket`.

### Output (before returning to client)

Responses are filtered before reaching your app. Sensitive words are replaced with `***`.

Streaming SSE responses are buffered across chunk boundaries and split on the SSE event terminator before each event's `delta.content` is filtered. This handles backends that pack multiple events into a single TCP chunk or split a single event across chunks. Non-data lines (`event:`, comments, `[DONE]`) pass through unchanged.

### Spotlighting (indirect injection defense)

Spotlighting marks untrusted content — typically RAG chunks delivered in `tool` or `function` role messages — so the LLM is reminded to treat it as data, not instructions. nanoguard ships three transforms:

- `datamarking` (default): replaces whitespace inside untrusted content with a marker character (e.g. `^`) so the region is visually obvious as preprocessed data.
- `delimiting`: wraps content with `<<UNTRUSTED>>`...`<</UNTRUSTED>>` markers.
- `encoding`: base64-encodes the content (strongest isolation, lowest answer quality — opt-in).

A system rider is automatically injected so the model knows what the markers mean. Without the rider the wrapping is security theater.

```toml
[input.spotlight]
enabled = true
method = "datamarking"           # "datamarking" | "delimiting" | "encoding"
untrusted_roles = ["tool"]
```

User and system messages are left untouched; only the listed roles are transformed. Spotlighting runs after PII redaction, so reversible-redaction placeholders are already in place when datamarking applies.

### Shadow mode

Set `[input] shadow = true` to scan and audit every request without actually blocking. A request that *would* have been blocked passes through to the backend, but the audit log records the verdict as `flag` with the matched rule prefixed `shadow_block:`. Useful when rolling out a new rule set in production — you can confirm the false-positive rate before enforcing.

```json
{
  "verdict": "flag",
  "matched_rule": "shadow_block:jailbreak",
  ...
}
```

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
shadow  = false  # set true to log "would-block" without enforcing — see Shadow mode

[input.keyword]
engine = "aho-corasick"  # "aho-corasick" (default) or "iword-rs"
dict_paths = []  # additional .txt word list files
inline_block = ["ignore previous instructions", "jailbreak"]
inline_alert = ["password", "api_key"]
inline_flag  = ["bitcoin", "crypto"]

[input.keyword.normalize]
nfkc       = true   # full-width → half-width, combining char folding
zero_width = true   # strip U+200B/C/D, U+FEFF
separators = false  # opt-in: collapse "j-a-i-l-b-r-e-a-k" → "jailbreak"
leet       = false  # opt-in: 3→e, 0→o, 1→i, 4→a, 5→s, 7→t, @→a, $→s

[input.pii]
enabled = true
action = "mask"  # mask | reject | log

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

### Budget tracking

Budget tracking uses the OpenAI `user` field as the budget key identifier, falling back to `default` when `user` is omitted. Usage is recorded from non-streaming backend responses that include OpenAI-compatible `usage.prompt_tokens` and `usage.completion_tokens`.

Streaming responses also record token usage when the client sets OpenAI's `stream_options: {"include_usage": true}` — the final chunk carries `usage`, which nanoguard captures and forwards to the budget store after the stream completes. Backends or clients that omit the option will continue to stream without spend accounting.

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

nanoguard dictionary format — tab-separated:

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
| Input check — clean | ~0.5 µs |
| Input check — blocked (early exit) | ~0.2 µs |
| Input check — long clean (~2,700 chars) | ~65 µs |
| Output filter — no match | ~4 µs |
| Output filter — mask SSN + credit card | ~7 µs |

Input scanning is O(N) in prompt length for literal rules. Regex rules are evaluated after literal BLOCK checks.

---

## Security

nanoguard minimizes external dependencies by design.
All filtering runs in-process with no network calls and no dynamic code at runtime.
The release binary is statically linked — what you audit is what runs.

### Limitations

nanoguard is a **policy enforcement layer**, not an adversarial-resistant security boundary.
The default normalization (NFKC + zero-width stripping) and opt-in transforms (`separators`,
`leet`) catch common obfuscation patterns, but rule-based filtering will always lose to a
sufficiently motivated attacker using paraphrasing, base64, multilingual variants, or novel
encodings. It is designed for:

- Compliance and audit trails
- Preventing accidental misuse (prompt injection from untrusted content)
- Enforcing organizational policy on LLM usage

It is **not** designed to defeat adversarial users who are actively trying to circumvent the filter.
For threat models that include motivated attackers, combine nanoguard with additional controls.

Streaming output filtering is per SSE event. A sensitive token that the backend genuinely splits across two events (e.g. `"ss"` then `"n"`) will not be detected; combining nanoguard with full-response scanning is recommended for high-assurance use cases.

---

## Credits

Input literal matching uses [aho-corasick](https://docs.rs/aho-corasick/). Legacy `iword-rs` support remains available as an alternate engine.
