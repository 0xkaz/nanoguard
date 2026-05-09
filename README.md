# nanoguard

[![CI](https://github.com/0xkaz/nanoguard/actions/workflows/ci.yml/badge.svg)](https://github.com/0xkaz/nanoguard/actions/workflows/ci.yml)
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

### With Ollama (local, no API key needed)

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
