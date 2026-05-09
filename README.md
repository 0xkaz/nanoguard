# nanoguard

[![CI](https://github.com/0xkaz/nanoguard/actions/workflows/ci.yml/badge.svg)](https://github.com/0xkaz/nanoguard/actions/workflows/ci.yml)
[![Release](https://github.com/0xkaz/nanoguard/actions/workflows/release.yml/badge.svg)](https://github.com/0xkaz/nanoguard/actions/workflows/release.yml)
[![ghcr.io](https://img.shields.io/badge/ghcr.io-nanoguard-blue)](https://github.com/0xkaz/nanoguard/pkgs/container/nanoguard)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

**Nano-fast. CPU-only. Offline-first. LLM Guardrails Proxy.**

nanoguard is a lightweight LLM guardrails proxy written in Rust.
It sits between your LLM client and any OpenAI-compatible backend,
filtering prompts and responses in microseconds — no GPU, no cloud, no Python.

```
[Your App / Open WebUI / Telegram Bot]
        ↓  POST /v1/chat/completions
  ┌─────────────────────────────┐
  │         nanoguard           │
  │  Input Guardrails  (μs)     │  ← keyword filter, PII keywords
  │  Backend Router             │  ← Ollama / OpenAI-compatible backends
  │  Output Guardrails (μs)     │  ← sensitive word filter / masking
  └─────────────────────────────┘
        ↓
  [LLM Backend]
```

## Why nanoguard?

| | nanoguard | LLM Guard | NeMo | LiteLLM |
|---|---|---|---|---|
| Language | Rust | Python | Python | Python |
| Proxy | ✅ | ❌ | ❌ | ✅ |
| Offline | ✅ | ✅ | △ | ✅ |
| GPU-free | ✅ | △ | ❌ | ✅ |
| Single binary | ✅ | ❌ | ❌ | ❌ |
| Memory | &lt;50MB (target: ~10MB) | ~300MB+ | heavy | ~372MB |

## Quick start

### Docker (recommended — no build required)

```bash
# Pull and run — architecture is auto-detected (arm64 / amd64)
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

> Image is published to [ghcr.io/0xkaz/nanoguard](https://github.com/0xkaz/nanoguard/pkgs/container/nanoguard) on every tagged release.
> Supports `linux/amd64` and `linux/arm64` (Apple Silicon / AWS Graviton).

### With Ollama (build from source)

```bash
git clone https://github.com/0xkaz/nanoguard
cd nanoguard
make run   # starts Ollama if needed, pulls qwen3:0.6b, runs nanoguard on :8080
```

```bash
# Use it exactly like OpenAI
curl http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"qwen3:0.6b","messages":[{"role":"user","content":"Hello!"}]}'
```

To use a different model:

```bash
make run MODEL=llama3.2:3b
```

### With OpenAI

```bash
export OPENAI_API_KEY=sk-...
export BACKEND_PROVIDER=openai
export BACKEND_ENDPOINT=https://api.openai.com
export BACKEND_MODEL=gpt-4o-mini

make run
```

### With a config file

```bash
cp nanoguard.toml myconfig.toml
# Edit myconfig.toml as needed
NANOGUARD_CONFIG=myconfig.toml make run
```

## Endpoints

| Endpoint | Description |
|----------|-------------|
| `POST /v1/chat/completions` | OpenAI-compatible chat — proxied through guardrails |
| `GET /v1/models` | List models from backend |
| `GET /health` | Health check |
| `GET /v1/admin/budget/:api_key` | Get token usage and limit (requires admin key) |
| `PUT /v1/admin/budget/:api_key` | Set token limit `{"limit": 100000}` (requires admin key) |
| `DELETE /v1/admin/budget/:api_key/reset` | Reset usage counter (requires admin key) |

## Guardrails

### Input (before sending to LLM)

Requests are scanned using [iword-rs](https://github.com/0xkaz/iword-rs) — a single-pass O(N) keyword engine.

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

LLM responses are filtered before reaching your app.
Sensitive words (`ssn`, `credit card`, etc.) are replaced with `***`.

## Configuration

`nanoguard.toml` (or set via environment variables):

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
dict_paths = []  # load additional .txt dict files
inline_block = ["ignore previous instructions", "jailbreak"]
inline_alert = ["password", "api_key"]
inline_flag  = ["bitcoin", "crypto"]

[output]
enabled = true
```

### Environment variables (no config file needed)

| Variable | Default | Description |
|----------|---------|-------------|
| `NANOGUARD_CONFIG` | `nanoguard.toml` | Config file path |
| `BACKEND_PROVIDER` | `ollama` | `ollama` / `openai` / any OpenAI-compatible |
| `BACKEND_ENDPOINT` | `http://localhost:11434` | Backend base URL |
| `BACKEND_API_KEY` | — | API key (also reads `OPENAI_API_KEY`) |
| `BACKEND_MODEL` | — | Default model name |
| `RUST_LOG` | `info` | Log level |
| `ADMIN_API_KEY` | — | Enables `/v1/admin/budget/*` endpoints with Bearer auth |

## Build

```bash
make          # release binary → target/release/nanoguard
make dev      # debug build + run
make test     # cargo test
make check    # clippy + fmt
```

Requires: Rust 1.75+

## Dictionary files

Load custom word lists in the same format as [iword-rs dicts](https://github.com/0xkaz/iword-rs/tree/main/dicts):

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

## Performance

Measured on Apple M-series (single core, release build). These are the guardrail hot-path costs — not including network round-trip to the LLM.

| Operation | Time |
|---|---|
| Input check — clean (no match) | ~10 µs |
| Input check — blocked (early exit) | ~7 µs |
| Input check — multiline normalization + block | ~7 µs |
| Output filter — no sensitive words | ~4 µs |
| Output filter — mask SSN + credit card | ~7 µs |

Input scanning is O(N) in prompt length. A 2,700-character prompt costs ~950 µs on the same machine.

Run benchmarks locally:

```bash
make bench   # criterion HTML report → target/criterion/
```

## Use cases

- **Local LLM protection** — wrap Ollama with guardrails for team use
- **Air-gapped environments** — factory, medical, government networks with no cloud
- **Bot safety layer** — protect Telegram / Slack bots from prompt injection
- **Cost control** — block off-topic or abusive prompts before they reach paid APIs

## Security policy

nanoguard is written in Rust and intentionally minimizes external dependencies.
All core filtering runs in-process with no network calls, no runtime scripts, and no package manager at deploy time.
The release artifact is a single statically-linked binary — what you audit is what runs.

## Credits

Powered by [iword-rs](https://github.com/0xkaz/iword-rs) — Pure Rust O(N) keyword search.
