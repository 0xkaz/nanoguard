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
        ↓  POST /v1/chat/completions  or  /v1/messages
  ┌──────────────────────────────────────┐
  │              nanoguard               │
  │  Input Guardrails  (<1 µs literal)   │  ← keyword / PII regex / injection / spotlight
  │  Policy Engine                       │  ← YAML rule bundles (ids / severity / compliance)
  │  Backend Router                      │  ← Ollama / OpenAI / Anthropic / any
  │  Output Guardrails (~4 µs)           │  ← word masking, streaming SSE
  │  Tool Gate + JSON Schema             │  ← allow / deny / validate tool_calls
  │  Audit Log                           │  ← JSONL, SHA-256 hash-only mode
  │  Hot Reload                          │  ← SIGHUP → atomic config swap
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

The image runs as the distroless `nonroot` user (UID 65532). The bundled `nanoguard.toml` writes its audit log to `/app/nanoguard-audit.jsonl` (and, if `[budget].enabled = true`, a SQLite file to `/app/nanoguard.db`). If you want to run the container with `--read-only`, either disable `[audit]` / `[budget]`, mount a writable volume over `/app`, or override the paths in your own config to a writable location.

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

## Manually verifying recent features

A 10-minute walkthrough that exercises the three feature areas that landed most recently — Anthropic-shape proxying with budget, Web Console user-management edits, and `[backends.*]` hot reload with 5xx fallback. The flow is end-to-end via `curl` against a single-process `nanoguard` (proxy + Console in one binary, listening on different ports). Replace any `qwen3:0.6b` with a model your backend actually has.

### 0. Launch

```bash
# A throwaway config that won't collide with whatever runs on :8080
cat > /tmp/nanoguard-verify.toml <<'EOF'
[nanoguard]
listen = "127.0.0.1:18080"
log_level = "info"

[backends.ollama]
provider = "ollama"
endpoint = "http://localhost:11434"
model    = "qwen3:0.6b"

[routing]
default = "ollama"

[input]
enabled = true

[input.keyword]
engine = "aho-corasick"
dict_paths = []
inline_block = ["ignore previous instructions"]
inline_alert = []
inline_flag  = []

[input.pii]
enabled = false
action  = "log"

[output]
enabled = false

[budget]
enabled       = true
db_path       = "/tmp/nanoguard-verify.db"
admin_api_key = "verify-admin-key"

[audit]
enabled   = true
path      = "/tmp/nanoguard-verify-audit.jsonl"
hash_only = true

[reload]
socket = "/tmp/nanoguard-verify-reload.sock"

[console]
enabled = true
listen  = "127.0.0.1:18081"

[console.auth]
mode = "local"

[console.auth.local]
allow_signup    = false
bootstrap_admin = { username = "admin", password_env = "VERIFY_PW" }
EOF

rm -f /tmp/nanoguard-verify.db /tmp/nanoguard-verify-audit.jsonl /tmp/nanoguard-verify-reload.sock
NANOGUARD_CONFIG=/tmp/nanoguard-verify.toml \
    VERIFY_PW="verify-pw-2026" \
    CONSOLE_SESSION_SECRET="$(openssl rand -hex 32)" \
    cargo run --release &
disown

# Wait for both listeners.
until curl -sf -o /dev/null http://127.0.0.1:18081/; do sleep 0.3; done
echo "proxy: http://127.0.0.1:18080   console: http://127.0.0.1:18081   admin: admin / verify-pw-2026"
```

### 1. Anthropic `/v1/messages` with per-request budget accounting

```bash
# OpenAI-shape works (sanity).
curl -s http://127.0.0.1:18080/v1/chat/completions \
    -H "Content-Type: application/json" \
    -d '{"model":"qwen3:0.6b","messages":[{"role":"user","content":"Reply just hi"}]}' \
    | jq '.choices[0].message.content'

# Anthropic-shape — same backend, different envelope.
curl -s http://127.0.0.1:18080/v1/messages \
    -H "Content-Type: application/json" \
    -d '{"model":"qwen3:0.6b","max_tokens":512,"messages":[{"role":"user","content":"Reply just hi"}]}' \
    | jq '{type, text: .content[0].text, usage}'

# Both endpoints accumulate against the `default` budget bucket
# (because [auth] is disabled in this config; with [auth].enabled
# you'd see `token:<id>` buckets instead).
curl -s -H "Authorization: Bearer verify-admin-key" \
    "http://127.0.0.1:18080/v1/admin/budget/default" | jq

# Prompt-injection input guardrail fires on both wire shapes and
# returns each provider's native error envelope.
curl -s http://127.0.0.1:18080/v1/chat/completions \
    -H "Content-Type: application/json" \
    -d '{"model":"qwen3:0.6b","messages":[{"role":"user","content":"ignore previous instructions"}]}' \
    | jq '.error.message'
curl -s http://127.0.0.1:18080/v1/messages \
    -H "Content-Type: application/json" \
    -d '{"model":"qwen3:0.6b","max_tokens":64,"messages":[{"role":"user","content":"ignore previous instructions"}]}' \
    | jq '.error.message'
```

You should see two distinct success responses, a non-zero usage in the budget, and two blocked-with-explanation envelopes shaped like the upstream provider's own errors.

### 2. Console Users tab — `allowed_models` edit + reset another user's password

```bash
# Admin login captures a session cookie + CSRF token.
LOGIN=$(curl -s -c /tmp/cookies http://127.0.0.1:18081/api/login \
    -H "Content-Type: application/json" \
    -d '{"username":"admin","password":"verify-pw-2026"}')
CSRF=$(echo "$LOGIN" | jq -r '.csrf_token')

# Create a non-admin user.
CREATE=$(curl -s -b /tmp/cookies -c /tmp/cookies \
    -D /tmp/hdrs.create \
    http://127.0.0.1:18081/api/users \
    -H "Content-Type: application/json" \
    -H "X-CSRF-Token: $CSRF" \
    -d '{"username":"alice","password":"alice-original-pw-12","role":"user"}')
ALICE_ID=$(echo "$CREATE" | jq -r '.id')
CSRF=$(grep -i '^x-csrf-token-next:' /tmp/hdrs.create | awk '{print $2}' | tr -d '\r')

# Edit her allowed_models. The DB stores a JSON-array string; the
# Console UI does the same wire format.
curl -s -b /tmp/cookies -c /tmp/cookies \
    -D /tmp/hdrs.edit \
    -X PUT http://127.0.0.1:18081/api/users/$ALICE_ID \
    -H "Content-Type: application/json" \
    -H "X-CSRF-Token: $CSRF" \
    -d '{"allowed_models":"[\"qwen3:0.6b\",\"claude-3-5-sonnet\"]"}'
CSRF=$(grep -i '^x-csrf-token-next:' /tmp/hdrs.edit | awk '{print $2}' | tr -d '\r')

# Confirm it persisted.
curl -s -b /tmp/cookies http://127.0.0.1:18081/api/users \
    | jq '.data[] | select(.username == "alice") | .allowed_models'

# Reset her password. Replaces the hash AND wipes every active
# session for her in one transaction — the point being that the
# original session cookie an attacker may already hold stops working
# the same instant the new password takes effect.
curl -s -o /dev/null -c /tmp/alice-cookies \
    http://127.0.0.1:18081/api/login \
    -H "Content-Type: application/json" \
    -d '{"username":"alice","password":"alice-original-pw-12"}'   # baseline session

curl -s -b /tmp/cookies -c /tmp/cookies \
    -X POST http://127.0.0.1:18081/api/users/$ALICE_ID/reset-password \
    -H "Content-Type: application/json" \
    -H "X-CSRF-Token: $CSRF" \
    -d '{"password":"alice-fresh-pw-67890"}' | jq

# Old password is now 401; new password works; old session is dead.
curl -s -o /dev/null -w "old-pw login: %{http_code}\n" \
    http://127.0.0.1:18081/api/login \
    -H "Content-Type: application/json" \
    -d '{"username":"alice","password":"alice-original-pw-12"}'
curl -s -o /dev/null -w "new-pw login: %{http_code}\n" \
    http://127.0.0.1:18081/api/login \
    -H "Content-Type: application/json" \
    -d '{"username":"alice","password":"alice-fresh-pw-67890"}'
curl -s -o /dev/null -w "stale cookie /api/me: %{http_code}\n" \
    -b /tmp/alice-cookies http://127.0.0.1:18081/api/me
```

Expect `200 / 401 / 200 / 401`. The two endpoints exist mainly to retire the offline `nanoguard-admin set-password` CLI for online use; the CLI stays as the no-Console recovery path.

### 3. `[backends.*]` hot reload + per-rule 5xx fallback

```bash
# Refresh CSRF after the password reset.
CSRF=$(curl -s -b /tmp/cookies http://127.0.0.1:18081/api/me | jq -r '.csrf_token')

# Add a second backend pointing at a port nothing listens on. The
# response should report restart_required=false and reload.triggered=true:
# the live pool now contains both `ollama` and `broken` without a
# process restart.
curl -s -b /tmp/cookies -c /tmp/cookies \
    -D /tmp/hdrs.addbe \
    "http://127.0.0.1:18081/api/backends?name=broken" \
    -H "Content-Type: application/json" \
    -H "X-CSRF-Token: $CSRF" \
    -d '{"provider":"openai","endpoint":"http://127.0.0.1:1","model":"qwen3:0.6b"}' \
    | jq '{name, restart_required, reload}'
CSRF=$(grep -i '^x-csrf-token-next:' /tmp/hdrs.addbe | awk '{print $2}' | tr -d '\r')

# Add a routing rule that sends `qwen3:0.6b` to the broken backend
# first and `ollama` as the fallback. The wire format supports a
# comma-separated `fallback` list per rule.
curl -s -b /tmp/cookies -c /tmp/cookies \
    -X PUT http://127.0.0.1:18081/api/routing \
    -H "Content-Type: application/json" \
    -H "X-CSRF-Token: $CSRF" \
    -d '{"default":"ollama","rules":[{"model":"qwen3:0.6b","backend":"broken","fallback":["ollama"]}]}' \
    | jq '.rules'

# Send a request. The primary `broken` returns a connection error,
# the proxy walks the fallback chain, `ollama` answers. From the
# caller's perspective: the request just succeeded. From the proxy
# log: one "backend `openai` errored on /v1/chat/completions" warn,
# then the upstream's response is surfaced as-is.
curl -s --max-time 60 http://127.0.0.1:18080/v1/chat/completions \
    -H "Content-Type: application/json" \
    -d '{"model":"qwen3:0.6b","messages":[{"role":"user","content":"Reply just hi"}]}' \
    | jq '.choices[0].message.content'

# Drop the rule, delete the broken backend — both hot-reload.
CSRF=$(curl -s -b /tmp/cookies http://127.0.0.1:18081/api/me | jq -r '.csrf_token')
curl -s -b /tmp/cookies -c /tmp/cookies \
    -X PUT http://127.0.0.1:18081/api/routing \
    -H "Content-Type: application/json" \
    -H "X-CSRF-Token: $CSRF" \
    -d '{"default":"ollama","rules":[]}' > /dev/null
CSRF=$(curl -s -b /tmp/cookies http://127.0.0.1:18081/api/me | jq -r '.csrf_token')
curl -s -b /tmp/cookies -c /tmp/cookies \
    -X DELETE http://127.0.0.1:18081/api/backends/broken \
    -H "X-CSRF-Token: $CSRF" | jq

# Audit-log spot check: every console mutation lands in the file.
python3 -c "
import json, sys
for line in open('/tmp/nanoguard-verify-audit.jsonl').readlines()[-15:]:
    try:
        d = json.loads(line)
        print(f\"{d.get('timestamp','?')[:19]} {d.get('actor','?'):10} {d.get('action','?')} target={d.get('target','-')}\")
    except: pass
"
```

You should see the routing PUT return a `200` chat response and the audit file carry `user_create`, `user_update`, `user_password_reset`, `backend_create`, `routing_update`, `backend_delete` lines in order. If `reload.triggered` is `false` for any mutation, fall back to `[reload].pid_file` (SIGHUP) in the TOML — the socket trigger sometimes false-fails on macOS in single-process mode and is being tracked separately.

### Teardown

```bash
pkill -f "target/release/nanoguard"
rm -f /tmp/nanoguard-verify.toml /tmp/nanoguard-verify.db \
      /tmp/nanoguard-verify-audit.jsonl /tmp/nanoguard-verify-reload.sock \
      /tmp/cookies /tmp/alice-cookies /tmp/hdrs.*
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

### Tool gate

When the LLM emits `tool_calls` in a response, nanoguard inspects each call before it reaches your application. Calls can be allowed, denied, or sanitized.

- **Name allow / deny** with `*` wildcard support: `[tools] allow = ["search_*"]`, `[tools] deny = ["exec_*", "shell"]`.
- **JSON Schema validation** on tool arguments — a call with the wrong shape is denied.
- **Entity scan** on tool argument values: a call whose arguments contain a `reject_entities` PII type is denied; `mask_entities` types are redacted in place.

A denied call is removed from `tool_calls[]` and a synthetic message (or, on `/v1/messages`, a `nanoguard_denied_tools` block; on streaming, a `tool_call_denied` SSE event) surfaces the rejection to the caller. Streaming tool gate operates after the proxy buffers the full `tool_calls[]` delta sequence and the `finish_reason: tool_calls` arrives.

```toml
[tools]
enabled = true
allow = ["search_kb", "lookup_*"]
deny  = ["exec_*"]
reject_entities = ["AWS_ACCESS_KEY_ID", "ANTHROPIC_KEY"]
mask_entities   = ["EMAIL"]

[[tools.schemas]]
tool_name   = "search_kb"
schema_path = "schemas/search_kb.json"
```

### Output JSON Schema validation

When the LLM is expected to return structured JSON (for tool calls, structured outputs, etc.), nanoguard can validate the response against a JSON Schema (Draft 2020-12) before forwarding. Per-route and per-model rule selection is supported. Prose / markdown-fence wrappers are stripped before validation.

```toml
[output.schema]
enabled      = true
on_violation = "alert"   # "alert" | "reject" | "repair" (repair is a reserved stub)

[[output.schema.rules]]
endpoint      = "/v1/chat/completions"
model_pattern = "gpt-4o.*"
schema_path   = "schemas/user_card.json"
name          = "user_card"
```

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

See [`docs/design/audit-log-format.md`](docs/design/audit-log-format.md) for the full JSONL schema, including reload entries written when the proxy hot-reloads its configuration.

---

## Policy bundles

Inline rules in `nanoguard.toml` are fine for small deployments. For compliance work or for sharing rule sets across nanoguard instances, declarative YAML policy bundles let you carry stable rule ids, categories, severities, and compliance tags.

```yaml
# policies/default.yaml
version: 1
metadata:
  name: nanoguard default
  updated: "2026-05-10"

rules:
  - id: PI-001
    category: prompt_injection
    severity: high
    pattern: ignore previous instructions
    action: block

  - id: PII-001
    category: pii
    severity: medium
    pattern: '/[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}/'
    action: redact
    placeholder: EMAIL
    compliance: ["GDPR", "HIPAA"]
```

```toml
[policies]
bundle_path = "policies/default.yaml"
```

At startup (and on hot reload) the bundle's keyword rules merge into `[input.keyword]` and its `redact` rules merge into the redactor patterns. When a request matches a policy rule, the audit log entry gains `rule_id` / `category` / `severity` / `compliance` fields so downstream consumers can pivot by rule lineage. The matcher / redactor hot path is unaffected — the policy index is consulted only at audit time.

See [`docs/design/policy-engine.md`](docs/design/policy-engine.md) for the full schema.

---

## Hot reload

Edit `nanoguard.toml`, the dict files, or the policy bundle, then signal the running process:

```bash
kill -HUP $(pidof nanoguard)
# or under systemd:
systemctl reload nanoguard
```

The matcher, redactor, spotlight transform, JSON Schema validators, tool gate, and policy index are rebuilt and atomically swapped in. In-flight requests finish on the snapshot they acquired at request entry; the next request sees the new state.

Reload is **all-or-nothing**. A failed parse, a regex that won't compile, an invalid JSON Schema — any failure leaves the live state untouched and writes a `reload_failed` audit entry. A successful reload writes `reload_ok`.

Restart-only knobs (the proxy keeps running on the previous values if these change in the file):

- `[nanoguard] listen` and `log_level`
- `[backend] *`
- `[budget] db_path`
- `[audit] path`

See [`docs/operations.md`](docs/operations.md) for systemd integration, log rotation, and diagnostics, and [`docs/design/hot-reload.md`](docs/design/hot-reload.md) for the design rationale.

---

## Multiple backends and routing

A single proxy can expose several upstream LLM backends side-by-side. Configure them under `[backends.NAME]` (operator-chosen label) and add a `[routing]` block to decide which backend handles which `model` name. The legacy single `[backend]` section still works; nanoguard synthesizes a `default` pool entry from it.

```toml
[backends.openai]
provider = "openai"
endpoint = "https://api.openai.com"
api_key  = "sk-..."

[backends.local]
provider = "ollama"
endpoint = "http://localhost:11434"

[backends.anthropic]
provider = "anthropic"
endpoint = "https://api.anthropic.com"
api_key  = "..."

[routing]
default = "local"     # used when no rule matches
rules = [
    { model = "gpt-*",     backend = "openai"    },
    { model = "claude-*",  backend = "anthropic" },
    { model = "qwen*",     backend = "local"     },
    { model = "llama*",    backend = "local"     },
]
```

Rules support exact strings and trailing-`*` globs (no other wildcards). They are scanned in the order declared; first match wins. A request whose `model` does not match any rule falls back to `[routing].default`.

The Console **Backends** tab (admin only) shows the current pool, the routing table, and lets you add or remove entries from a UI. The same tab carries a **Routing** section that edits `[routing]` (default backend + first-match-wins rules) with ↑ / ↓ buttons to reorder rules — order is significant. The mutation rewrites `nanoguard.toml` on disk via a TOML round-trip (unrelated sections and comments preserved). The proxy's live backend pool is **restart-only** — see `docs/design/multi-backend-routing.md > State management` for the reason — so a freshly-added backend is visible on disk and in the API immediately, but the proxy itself only picks it up on the next process restart. Routing rules and the default selection are hot-reloadable (saving the Routing section fires a reload immediately).

What is NOT in this iteration:

- Per-client `allowed_models` (token-scoped permission). The current implementation lets any authenticated caller hit any backend the routing table reaches.
- Provider-side failover (auto-retry on a different backend when the primary returns 5xx). nanoguard does **not** silently re-route; downed-backend requests fail.
- Per-backend quota partitioning. Budgets remain global per token.

See `docs/design/multi-backend-routing.md` for the full design.

---

## Web Configuration UI

The `nanoguard` binary spawns the operator console alongside the proxy on startup. `make run` brings up **both** the proxy (`:8080`) and the console (`:8081`) on a single process — there is no separate console binary or `make run-console`. Set `[console].enabled = false` in `nanoguard.toml` for headless deployments. **The proxy itself never exposes a mutation HTTP surface** — every write goes through the console listener which has its own port and its own auth — see [`docs/design/web-config-ui.md`](docs/design/web-config-ui.md) for the rationale.

Phase 1 (commit `cdc795c`) shipped read-only browsing of audit log, budget state, and the current config, plus self-service proxy-token issue/revoke for the logged-in user and admin user CRUD (allowed models, budget limit, role, enable/disable). Phase 2 (commit `ef5b188`) added file-based config editing for `nanoguard.toml`, `dicts/*.txt`, and `policies/*.yaml`: every write is validated server-side with the same parsers the proxy runs at reload time, atomically renamed into place, backed up under `.nanoguard-backups/` (per-file retention configurable via `[console] backup_limit`, default 20), audited to `console-audit.jsonl`, and followed by a reload trigger (`SIGHUP` via `[reload] pid_file`, or `RELOAD\n` over `[reload] socket`). OIDC login and per-session CSRF tokens are Phase 4 and remain on the roadmap.

### 1. Configure `nanoguard.toml`

The default `nanoguard.toml` ships with the `[console]` section already enabled. `session_secret = ""` is fine for local development — leave it empty and `nanoguard` will generate a random ephemeral secret at startup. To persist sessions across restarts, set a random string of at least 32 bytes (the underlying `cookie::Key` requires it; `nanoguard` fails fast with a clear error otherwise) — `openssl rand -hex 32` produces a suitable value. You can put it directly in TOML or export `CONSOLE_SESSION_SECRET`:

```toml
[console]
listen            = "127.0.0.1:8081"           # loopback only; non-loopback auto-enables Secure cookies
session_secret    = ""                          # empty => ephemeral secret generated at startup; set a long random string to persist
session_ttl_hours = 24
# audit_path        = "console-audit.jsonl"    # Phase 2: where the admin-edit log is written
# backup_limit      = 20                       # Phase 2: per-file cap under .nanoguard-backups/ (0 disables pruning)

[console.auth]
mode = "local"                              # Phase 1 supports "local" only; OIDC is Phase 4

[console.auth.local]
allow_signup    = false
bootstrap_admin = { username = "admin", password_env = "BOOTSTRAP_PASSWORD" }
```

If `CONSOLE_SESSION_SECRET` is set in the environment and non-empty, it overrides `[console].session_secret`. If both are empty, `nanoguard` generates a random ephemeral secret and logs a `WARN` line at startup. This is convenient for testing but logs all users out on restart. TOML does not perform `${VAR}` expansion — pass secrets via env, not via `"${VAR}"` strings in the TOML file.

If `listen` is non-loopback, cookies are automatically marked `Secure` — terminate TLS in front of the console in that case.

### 2. Start the console

`make run` boots both the proxy (:8080) and the console (:8081) on a single `nanoguard` process — the console listener is spawned inline whenever `[console].enabled = true` (the default). For local development:

```bash
make run
# `make run` itself auto-generates CONSOLE_SESSION_SECRET and a one-shot
# BOOTSTRAP_PASSWORD when the users table is empty, and prints the admin
# password once on stdout. To pin them yourself instead:
#   export CONSOLE_SESSION_SECRET="$(openssl rand -hex 32)"
#   export BOOTSTRAP_PASSWORD='your-initial-admin-password'
#   make run
```

`BOOTSTRAP_PASSWORD` is read **before** the tokio runtime starts and wrapped in `Zeroizing<String>` so the in-memory plaintext is overwritten after hashing. The bootstrap is **one-shot** — the user table is only seeded when it is empty. After the first start logs `Bootstrap admin '<name>' provisioned. Clear $BOOTSTRAP_PASSWORD from the environment.`, **unset `BOOTSTRAP_PASSWORD` in your shell**; on subsequent restarts that env var is ignored. The `Zeroizing` wrap kills the in-process copy, but the env slot in `/proc/<pid>/environ` itself persists until you `unset` — clear it after first boot.

For headless deployments where you do not want a management UI exposed at all, set `[console].enabled = false` in `nanoguard.toml`. The `nanoguard` binary then runs proxy-only.

### 2a. Recovering a forgotten admin password

If the admin password is lost — for example because `make run` generated and printed it once and the line is no longer in the terminal scrollback — the offline `nanoguard-admin` CLI can reset any user's password without going through the web UI:

```bash
make set-admin-password                       # interactive prompt with echo off
make set-admin-password ADMIN_USER=alice      # target a different user

# or directly:
./target/release/nanoguard-admin set-password admin            # tty prompt
./target/release/nanoguard-admin list-users                    # see who exists
echo "$NEW_PW" | ./target/release/nanoguard-admin set-password admin --password-stdin
```

The plaintext password is wrapped in `Zeroizing<String>` while it lives in memory, hashed with the same argon2id parameters the console itself uses, and **existing sessions for the affected user are invalidated** so a leaked cookie cannot keep an attacker signed in. `nanoguard-admin` reads the same `[budget].db_path` the console reads, so run it from the same working directory.

### 3. Open the console

Browse to `http://127.0.0.1:8081/` and log in as the bootstrap admin.

The proxy and the console are independent processes; you can run the console without the proxy and vice versa. They share `[budget].db_path` (default `nanoguard.db`), so run both from the same working directory.

### 3a. Playground tab (admin only)

The **Playground** tab is a two-pane debugging surface that answers the recurring question "is nanoguard blocking this, or is my backend returning garbage?" — paste an OpenAI chat-completions request body, pick a backend from the dropdown, then click either "Send through proxy" (full guardrail pipeline) or "Send direct to backend" (raw upstream, no guardrails) and read both responses side by side. Status and round-trip latency render next to the response body so you can see whether the call actually round-tripped. The proxy-direction call optionally takes a Bearer token field for testing `[auth].enabled = true` setups; the backend-direction call pulls `api_key` from the live `[backends.*]` config — admins cannot exfiltrate keys through this endpoint. Every Playground call is audited under `playground_proxy` / `playground_backend` in `console-audit.jsonl`, but the **request body is deliberately not logged** because operators routinely paste secrets while testing redaction.

### 4. Point your app at the proxy

The Overview tab in the Console renders a **Getting Started** panel with the live proxy URL, a working curl example, and the OpenAI SDK env-var form. The same information assembled by hand:

- **Proxy URL:** `http://<your-host>:8080` (from `[nanoguard].listen`)
- **Endpoints:**
  - `POST /v1/chat/completions` — OpenAI-compatible
  - `POST /v1/messages` — Anthropic-compatible (text + `tool_use`)
  - `GET /v1/models` — passthrough
  - `GET /health` — unauthenticated
- **Auth:** if `[auth].enabled = true` in `nanoguard.toml`, every `/v1/*` request needs `Authorization: Bearer <token>`. Mint a token from the **Tokens** tab in the console; the wire value is shown once on creation.

curl, with `[auth].enabled = true`:

```bash
curl http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer ng_t_..." \
  -d '{"model":"qwen3:0.6b","messages":[{"role":"user","content":"hello"}]}'
```

OpenAI SDKs are drop-in — just override the base URL:

```bash
# Python / Node
export OPENAI_BASE_URL=http://localhost:8080/v1
export OPENAI_API_KEY=ng_t_...   # one of your tokens from the Tokens tab
```

The Overview tab also renders a **Guards Active** panel listing which input/output guards are on (keyword block list, PII redaction, spotlighting, schema validation, tool gate, policy bundle) and their current parameters. Most are reload-safe — edit `nanoguard.toml` from the Config tab and the proxy picks up the new state without a restart.

---

## Configuration

`nanoguard.toml`:

```toml
[nanoguard]
listen = "0.0.0.0:8080"
log_level = "info"

# Single-backend deployments use [backend] (legacy schema):
[backend]
provider = "ollama"
endpoint = "http://localhost:11434"
# api_key = "sk-..."
# model = "llama3.2"

# Multi-backend deployments use [backends.NAME] instead (one section
# per upstream) plus a [routing] block. Mixing both is allowed during
# migration — [backends.*] takes precedence and a startup warning
# fires. See "Multiple backends" below and
# `docs/design/multi-backend-routing.md`.

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

# [output.schema] — see "Output JSON Schema validation" above
# [tools]         — see "Tool gate" above
# [policies]      — see "Policy bundles" above

[budget]
enabled = false
db_path = "nanoguard.db"
# admin_api_key = "your-secret-key"

[audit]
enabled = false
path = "nanoguard-audit.jsonl"
hash_only = true

# [console]                          — see "Web Configuration UI" above; spawned inline by `nanoguard` when [console].enabled = true
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
| `CONSOLE_SESSION_SECRET` | — | Optional. If set and non-empty, overrides `[console].session_secret`. If unset and the TOML value is empty, an ephemeral secret is generated at startup (sessions reset on restart). |
| `BOOTSTRAP_PASSWORD` | — | One-shot plaintext password for the bootstrap admin user; unset after first start |

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
make geiger   # unsafe code audit
make semgrep  # Semgrep CE security scan
make bench    # criterion benchmarks → target/criterion/
make mirai    # optional MIRAI static analysis
```

Requires: Rust 1.75+

PR CI runs the fast build/test/lint matrix plus MSRV, Trivy, and Semgrep for
code changes. Documentation-only PRs keep the same required check names green
but skip Rust build/test/lint, MSRV, audit, and coverage. `security audit` and
`test coverage` run in the scheduled/manual Nightly workflow so merges do not
wait on post-merge CI; run `make preflight` locally before opening code PRs.

Optional deep static analysis uses MIRAI. Install it once from the maintained
upstream repository:

```bash
git clone https://github.com/endorlabs/MIRAI.git
cd MIRAI
cargo install --locked --path ./checker
```

Then run `make mirai` from this repo. Use `make preflight-mirai` when you want
the normal preflight suite plus MIRAI before opening a security-sensitive PR.
You can pass MIRAI options through `MIRAI_FLAGS`, for example:

```bash
MIRAI_FLAGS="--diag=verify --body_analysis_timeout 60" make mirai
```

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
