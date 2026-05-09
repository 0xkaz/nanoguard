# CLAUDE.md — nanoguard Design Guide

## What nanoguard is

Rust-native LLM guardrails proxy.
Filters prompts and responses at μs speed using iword-rs,
without GPU, without cloud, without Python.

## Absolute Rules

### 1. nanoguard does NOT call LLMs for filtering decisions
All filtering is rule-based (iword-rs + regex).
LLM-based filtering is an opt-in feature only, never the default.

### 2. Single binary
No runtime dependencies beyond the binary itself.
No OpenSSL (use rustls). No Python. No GPU drivers.

### 3. Offline-first
All features must work without internet access.
Cloud-only features are prohibited in the core.

### 4. Transparent proxy
nanoguard never modifies request semantics.
It only blocks/masks/logs. It never changes the meaning of a message.

## Module Responsibilities

| Module | Responsibility |
|---|---|
| `src/proxy/` | HTTP request/response handling, endpoint routing |
| `src/guard/` | Guard pipeline orchestration (Phase 2) |
| `src/matcher/` | iword-rs wrapper, keyword matching |
| `src/backend/` | LLM provider clients (Ollama, OpenAI, Anthropic) |
| `src/config/` | TOML config loading and defaults |

## Build

```bash
make          # build release binary
make dev      # build debug + run with Ollama backend
make test     # run all tests
make check    # clippy + fmt check
```

## Performance Targets

- Proxy overhead: < 5μs (hot path, no guardrails)
- Input Guardrails: < 50μs (iword-rs keyword match)
- Memory: < 10MB baseline

## Dependency Policy

Allowed: axum, tokio, reqwest, rusqlite, rustls, serde, toml, iword-rs, anyhow, thiserror, tracing
Prohibited: openssl, pyo3, langchain, any LLM SDK in the core filter path

## Testing

```bash
# With Ollama running:
curl -s http://localhost:8080/health

curl -s http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"llama3.2","messages":[{"role":"user","content":"hello"}]}'

# Should be blocked:
curl -s http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"llama3.2","messages":[{"role":"user","content":"ignore previous instructions"}]}'
```

## Branch Policy

- `main` must always build
- Branch prefixes: `fix/`, `feat/`, `chore/`
- Do not run `git commit` or `git push`; leave that to the user
