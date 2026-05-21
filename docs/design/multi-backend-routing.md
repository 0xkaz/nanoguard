> **Status:** partial (2026-05-20)

# Multi-Backend Routing

> ## Shipped vs. Proposed
>
> **Shipped in Phase 1** (this iteration):
> - `[backends.NAME]` schema, multi-pool runtime (one `reqwest` client per backend).
> - `[routing]` block with `default` + `rules`. Rule patterns: exact string or trailing-`*` glob. Rules scan in declared order, first match wins.
> - Legacy `[backend]` single-section auto-promoted to `[backends.default]` with synthesized routing default.
> - Required: `[routing].default` MUST be set whenever more than one backend is configured (no silent first-pick fallback).
> - Both `[backends.*]` and `[routing]` are hot-reloadable. The reload path rebuilds `BackendPoolRuntime` from the fresh config; in-flight requests hold their own `Arc<AppState>` snapshot, so old `reqwest::Client` pools stay alive until the last request drops. `Config::pool()` validates that every rule references a real backend and that `[routing].default` is one of them — invalid reloads are refused with a `reload_failed` audit entry.
> - Per-rule `fallback = [...]` lists are honored on `/v1/chat/completions` and `/v1/messages`. The proxy tries the primary; on network error or upstream status >= 500, walks the fallback list in order; commits to the first non-5xx response. Never fires mid-stream.
> - Console: admin-only Backends tab + `GET|POST /api/backends` and `PUT|DELETE /api/backends/:name`. `api_key` has 3-state semantics (omit = keep, null = clear, string = replace). The Routing editor surfaces `fallback` as a comma-separated input per rule.
>
> **Proposed for Phase 2+** (everything below in this document that talks about):
> - Per-client `allowed_models` and the audit verdicts `model_denied` / `model_unrouted`.
> - The map-style routing schema (`[routing] "gpt-4o" = "openai"`) — superseded by `[routing.rules]` in the shipped form.
> - `display_name` per backend, `/v1/models` allowlist intersection.
> - Health-checked / circuit-broken / cost-aware routing. The shipped `fallback` list is try-once on 5xx and no more.
>
> When reading the rest of this document, assume "shipped" means the bullets above and treat everything else as a future-state specification.

Today `nanoguard.toml` defines a single `[backend]` section. Every
request is forwarded to that one upstream — OpenAI, or Anthropic,
or an Ollama instance, but exactly one. In a multi-user deployment
this is too coarse: operators want to expose several upstreams from
the same proxy and restrict which users can reach which models.

This document defines a routing layer that sits between
authentication and the existing guardrail pipeline:

```
request → client-auth → model allowlist check → routing lookup → guardrails → backend
```

`docs/design/client-auth.md` defines how a request gets a
`ClientView`. This doc defines how the `ClientView.allowed_models`
field is populated, how the `model` field on the request body is
matched against it, and how a matched model is dispatched to one
of several configured backends.

## Goals

1. **Multiple upstream backends from a single proxy.** OpenAI,
   Anthropic, Ollama, DeepSeek, and any future OpenAI-compatible
   endpoint can be configured side-by-side.
2. **Per-client model permission.** A token issued for user Alice
   can call `gpt-4o-mini` and `qwen3:0.6b` but not `gpt-4o` or
   `claude-opus-4-7`. Permission denial is a 403, not a generic
   500 or a forwarded provider error.
3. **Model name → backend resolution is operator-controlled.** The
   operator declares which model name reaches which backend, not
   the client. A client cannot make `gpt-4o` reach a self-hosted
   look-alike by guessing endpoints.
4. **Backwards-compatible migration.** Existing deployments using
   the single `[backend]` section keep working; the new schema is
   read first, then falls back.

## Non-goals

- **Cost-aware, health-checked, or circuit-broken routing.** The
  shipped per-rule `fallback = [...]` list is the bare minimum: try
  primary, try each fallback once on 5xx or network error, surface.
  No health probes, no rolling counters, no smart load balancing,
  no per-backend retry budgets. Operators who want any of that
  layer it downstream — LiteLLM is the supported tool. See
  `docs/roadmap.md > Non-goals`.
- **Cost-based routing.** Picking the cheapest backend that
  satisfies a model alias is out of scope. Operators who want this
  can implement it client-side and route to a specific model.
- **Streaming protocol translation between providers.** Anthropic
  `/v1/messages` and OpenAI `/v1/chat/completions` keep their own
  request paths (the existing `src/proxy/anthropic.rs` adapter
  stays). Multi-backend routing is per-endpoint.
- **Dynamic backend discovery.** No service-discovery integration.
  Backends are declared statically in the config file.

## Configuration

### Backends

```toml
[backends.openai]
provider = "openai"
endpoint = "https://api.openai.com"
api_key  = "${OPENAI_API_KEY}"
timeout_secs = 60
# Optional per-backend overrides
default_model = "gpt-4o-mini"

[backends.anthropic]
provider = "anthropic"
endpoint = "https://api.anthropic.com"
api_key  = "${ANTHROPIC_API_KEY}"

[backends.local]
provider = "ollama"
endpoint = "http://localhost:11434"
# Ollama needs no api_key

[backends.deepseek]
provider = "openai"            # DeepSeek is OpenAI-compatible
endpoint = "https://api.deepseek.com"
api_key  = "${DEEPSEEK_API_KEY}"
```

Backend names (`openai`, `anthropic`, `local`, `deepseek` in the
example) are operator-chosen labels. The `provider` field tells
nanoguard which on-the-wire adapter to use (currently `openai`,
`anthropic`, or `ollama`).

### Routing

A flat map from model name (as the client sends it) to a backend
label:

```toml
[routing]
"gpt-4o"          = "openai"
"gpt-4o-mini"     = "openai"
"o1-preview"      = "openai"
"claude-opus-4-7" = "anthropic"
"claude-sonnet-4-6" = "anthropic"
"qwen3:0.6b"      = "local"
"llama3.2"        = "local"
"deepseek-chat"   = "deepseek"
```

A request with `"model": "gpt-4o-mini"` resolves to the `openai`
backend. A request with a model not in this map gets a 404 with
body `{"error": "model not configured"}` — even if the client's
allowlist would have permitted it. Operators must declare every
allowed model explicitly. Glob entries (`"gpt-4o-*"`) are
supported via a fallback pass after exact-match lookup fails:

```toml
[routing]
"gpt-4o-*"        = "openai"   # glob; checked after exact misses
"claude-*"        = "anthropic"
```

Glob semantics: simple `*` wildcard, no `?`, no character classes,
no nested globs. Exact matches always win over globs. Among
multiple matching globs, the longest static prefix wins
(deterministic; documented).

### Per-client allowlist

A user (and through them, every token they hold) gets an
`allowed_models` list:

```toml
# Surfaced via user-management.md; the file form is for bootstrap
# and testing. The console is the normal path.
[users.alice]
allowed_models = ["gpt-4o-mini", "qwen3:0.6b"]

[users.bob]
allowed_models = ["*"]                 # wildcard: every model in [routing]

[users.carol]
allowed_models = ["gpt-4o-mini", "claude-*"]   # exact + glob

[users.guest]
allowed_models = []                    # no LLM access; useful for budget-only roles
```

Resolution per request:

1. Pull `ClientView.allowed_models` (from the token's user record).
2. Read the `model` field from the request body.
3. Match against the allowlist:
   - Empty list → always 403.
   - `*` in list → allow if the model resolves in `[routing]`.
   - Exact match → allow.
   - Glob match → allow (same glob rules as routing).
4. If allowed, look up `[routing][model]` to get the backend label.
5. If the model is allowed but not present in `[routing]` →
   500 + `"error":"misconfigured: allowed model has no route"`.
   This is an operator misconfig, not a client error.

Step 5 fires only when an admin permitted a model name that has no
backend route. The console UI prevents this combination on save
(see `web-config-ui.md` revisions), so it should be rare; but
nanoguard treats it as a server error rather than silently
denying, so the operator hears about it.

## Endpoint behavior

### `/v1/chat/completions`

The model field is in the JSON body. Authentication and allowlist
check happen *before* keyword scanning — there is no reason to spend
guardrail cycles on a request that will be rejected on policy
grounds.

```
1. Authenticate (client-auth.md).
2. Parse the JSON body; extract `model`.
3. Allowlist check against ClientView.allowed_models.
   Disallowed → 403, audit verdict "model_denied".
4. Route lookup in [routing].
   Missing → 404, audit verdict "model_unrouted".
5. Existing guardrail pipeline (keyword, PII, spotlight, tool gate).
6. Forward to backends[label] using its endpoint + api_key.
```

The new audit verdicts (`model_denied`, `model_unrouted`) join the
existing set (`block`, `alert`, `flag`, `pass`, `reload_*`).

### `/v1/messages`

Anthropic format. The `model` field is at the top level of the
body, same shape. Same flow. The adapter is selected from the
backend's `provider`, not from the request endpoint: a client can
hit `/v1/messages` with `model: "gpt-4o"` and reach the OpenAI
backend through the Anthropic-to-OpenAI adapter. That capability
exists today via `src/proxy/anthropic.rs`; multi-backend routing
preserves it.

### `/v1/models`

Returns the **intersection** of the routing map and the
authenticated client's allowlist. An unauthenticated request (once
auth is enforced) gets 401.

```json
{
  "object": "list",
  "data": [
    {"id": "gpt-4o-mini", "object": "model", "owned_by": "openai"},
    {"id": "qwen3:0.6b",  "object": "model", "owned_by": "local"}
  ]
}
```

The `owned_by` field reflects the backend label, which leaks a
detail an operator might prefer to hide ("local" reveals it is a
self-hosted Ollama). The label is therefore configurable per
backend:

```toml
[backends.local]
provider     = "ollama"
endpoint     = "http://localhost:11434"
display_name = "in-house"   # shown as owned_by, defaults to backend label
```

### `/health`

Unchanged. No auth, no model semantics.

## Adapter selection

The `provider` field on each backend determines the wire-level
adapter:

- `"openai"` — pass-through for `/v1/chat/completions`. Used by
  OpenAI proper, DeepSeek, Together AI, Groq, and most OpenAI-
  compatible servers.
- `"anthropic"` — pass-through for `/v1/messages`. Used by
  Anthropic proper.
- `"ollama"` — pass-through for `/v1/chat/completions` (Ollama's
  OpenAI-compatible mode). Streaming format differences are
  already handled by `src/proxy/sse.rs`.

Cross-adapter calls go through the existing translation layer:

| Client hits ↓ \ Backend provider → | `openai`      | `anthropic`         | `ollama`     |
|------------------------------------|---------------|---------------------|--------------|
| `/v1/chat/completions`             | passthrough   | OpenAI→Anthropic    | passthrough  |
| `/v1/messages`                     | Anthropic→OpenAI | passthrough      | Anthropic→OpenAI (via OpenAI shape) |

The diagonal cells are already implemented today; the off-diagonals
reuse the same `src/proxy/anthropic.rs` adapter. Multi-backend
routing does not add new adapter code.

## State management

The routing table and the per-user allowlists are part of the
`ReloadableState` defined in `docs/design/hot-reload.md`. Editing
the routing table via the console fires the same SIGHUP-driven
atomic swap; in-flight requests finish on the snapshot they
acquired, the next request sees the new routing.

The backend pool (`reqwest::Client` per backend) is hot-reloadable.
On reload, the new `AppState` is built with a fresh
`BackendPoolRuntime` from the new config; in-flight requests hold
their own `Arc<AppState>` snapshot via `shared.load_full()`, so the
prior pool (and its `reqwest::Client` connection pools) stays alive
until the last in-flight request drops its Arc. The old pools are
dropped only after their last user finishes — no orphaning.

Per-user allowlists are hot-reloadable on the same swap.

## Audit and budget integration

- **Audit**. Each audited request gains a `model` and a `backend`
  field (both already extractable from the request flow; this just
  formalizes them). A `model_denied` or `model_unrouted` verdict
  records the requested model and skips `backend` (no routing
  happened).
- **Budget**. Buckets remain per-token (`token:<id>`); the
  request's resolved backend is recorded in audit but does not
  partition the budget. A token's quota is global across the
  models it is allowed to use. Per-(token, model) sub-budgets are
  a future feature gated by demand.

## Migration from single `[backend]`

The new and old schemas coexist for one release:

1. **Stage 1 — accept both.** If `[backends.*]` is present,
   ignore `[backend]` entirely. If only `[backend]` is present,
   nanoguard synthesizes a single `[backends.default]` entry from
   it and a routing table that maps every model in `[backend].
   model` or the request body's `model` to `default`. A startup
   log line records which schema was used.
2. **Stage 2 — deprecate `[backend]`.** A warning at startup for
   any deployment still using the old schema, with a one-line
   migration recipe.
3. **Stage 3 — remove `[backend]`.** Two releases after Stage 1.

The console's "current configuration" view shows both schemas in
the deprecation window so operators can see what nanoguard is
actually using.

## Failure modes

| Condition                                       | Response                                   | Audit verdict     |
|-------------------------------------------------|--------------------------------------------|-------------------|
| `model` field absent from request body          | 400, `"error":"missing model"`             | `bad_request`     |
| `model` not in client's allowed_models          | 403, `"error":"model not permitted"`       | `model_denied`    |
| `model` allowed but not in `[routing]`          | 500, `"error":"misconfigured"`             | `misconfig`       |
| `model` not allowed (or routing missing without allowlist match) | 404 + `"error":"model not configured"` | `model_unrouted` |
| Backend HTTP error (timeout, 5xx)               | 502, body forwards backend error           | `backend_error`   |
| Streaming backend error mid-stream              | Existing SSE error event                   | `backend_error`   |

The 403/404/500 split matters for client diagnostics: 403 says
"your token can't do this", 404 says "no such model on this proxy",
500 says "this proxy is broken." Conflating them produces an
unhelpful "permission denied for some reason" experience.

## Threat model

- **Model alias spoofing**. A client cannot send `model:
  "trusted-internal"` and reach an arbitrary endpoint; the operator
  controls `[routing]`. Allowing operator-chosen aliases (e.g.
  `"fast"` → `gpt-4o-mini`) is a feature, not a vulnerability.
- **Bypass via the body's `user` field**. Already discussed in
  `client-auth.md`: the OpenAI `user` field is forwarded but not
  used for any policy decision. The verified `ClientView` is the
  authority.
- **Backend exhaustion**. A token allowed to hit `openai` can burn
  the configured OpenAI quota. Budget caps mitigate; per-backend
  quotas are future work.
- **Information leak via `/v1/models`**. The endpoint reflects
  what the *authenticated* client can use; unauthenticated callers
  see nothing. There is no anonymous model discovery.

## Open questions

- **Per-token allowlists vs per-user allowlists**: currently
  per-user, with all tokens of a user sharing the list. A
  per-token allowlist is more flexible (a service token for
  cheap models, an interactive token for the full set). Direction:
  ship per-user first, add per-token override later if demand
  appears.
- **Model name normalization**: should `gpt-4o` and `GPT-4o` be
  treated as the same? OpenAI is case-sensitive in practice;
  doing nothing is the safe default. Operators who want
  case-insensitive matching can normalize in their routing keys.
- **Aliases as a first-class feature**: `[aliases] "fast" =
  "gpt-4o-mini"` so a client can say `model: "fast"` and get
  whatever the operator currently considers "fast." Useful, but
  additive; defer until the basic routing is in place.
- **Quota partitioning by backend**: a token's budget is currently
  global. A "max $X/day on OpenAI specifically" cap is sometimes
  asked for. Defer to a follow-up budget doc.
- **Provider matrix beyond OpenAI / Anthropic / Ollama**: the
  current adapter set covers OpenAI-compatible servers and
  Anthropic. Adding Google Gemini, AWS Bedrock, Cohere, Vertex,
  etc. is technically straightforward — the `provider` field on
  `[backends.*]` is exactly the extension point — but each new
  adapter doubles the surface area of the translation layer, the
  audit trail, and the dependency tree. Direction: if/when this
  work proceeds, gate each new provider behind a new Cargo feature
  added at that time (the crate has no `[features]` section today;
  the gate is a future constraint on how providers will be added,
  not a description of shipped behavior). The default binary
  continues to ship the three built-in providers it has today
  (openai, anthropic, ollama) so the
  `CLAUDE.md > Performance Targets > Memory: < 10MB` bar holds.
  Funnel all adapters through an OpenAI-shaped IR so we stay at
  N+N adapters instead of an N² translation matrix, and continue
  to recommend LiteLLM downstream as the default story for
  deployments that want 100+ providers. See
  [`docs/roadmap.md` → "Provider matrix expansion (post-v1,
  opt-in)"](../roadmap.md#provider-matrix-expansion-post-v1-opt-in)
  for the full design lines.
