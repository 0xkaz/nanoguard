> **Status:** proposed (2026-05-10)

# Edge Deployment Feasibility

This note summarizes whether nanoguard can be deployed at the edge, with a
focus on Cloudflare Workers and Durable Objects.

## Short Answer

The current Rust binary is not a direct fit for Cloudflare Workers.
The codebase assumes:

- a native Rust executable
- `axum` / `tokio` / `reqwest`
- local file-style config and dictionary loading
- SQLite-backed budget storage
- long-lived process semantics

That is a good fit for a VM, container, or small edge host.
It is not a drop-in Workers deployment.

## What Works Well at the Edge

nanoguard already has traits that make edge deployment plausible:

- small request path
- deterministic rule evaluation
- no GPU dependency
- no core LLM calls during filtering
- streaming-safe proxy behavior
- local budget and audit concepts

This makes the project a reasonable candidate for:

- edge VM / container deployment
- regional proxy deployment
- Cloudflare-fronted origin deployment

## Cloudflare Workers Constraints

Cloudflare Workers are a JavaScript / TypeScript runtime with fetch-based
request handling. They do support streaming and can coordinate state through
Durable Objects, but the current codebase does not map directly to that model.

Important constraints:

- native Rust binary is not the deployment unit
- SQLite in Workers is tied to SQLite-backed Durable Objects, not the Worker
  runtime itself
- file-based local dictionary loading does not translate directly
- `reqwest` / `tokio` / `axum` are not the runtime surface

Official Cloudflare docs currently recommend SQLite-backed Durable Objects for
stateful storage, while plain Workers remain a fetch-oriented execution model.

## Practical Deployment Patterns

### Pattern 1: Cloudflare Front Door, nanoguard Origin

This is the most realistic first step.

Flow:

`client -> Cloudflare -> nanoguard -> backend`

Use Cloudflare for:

- TLS termination
- caching of non-sensitive assets
- rate limiting / WAF
- global edge ingress

Keep nanoguard on:

- a small VM
- a container host
- a regional edge provider

This preserves the current Rust codebase.

### Pattern 2: Edge-Lite Reimplementation

If a true Workers-native deployment is needed, nanoguard would need an edge
variant with a narrower surface:

- fetch handler instead of `axum`
- Durable Objects for budget / state
- bundled dictionaries instead of local files
- no native SQLite dependency
- no native Rust binary assumption

That is feasible, but it is a separate product track rather than a simple port.

## Recommendation

Treat edge deployment as a two-stage strategy:

1. Ship the current Rust binary on a VM or container.
2. If demand exists, design a Workers-native edge-lite variant.

Do not block the current product on a Workers rewrite.
The main product value comes from deterministic local guardrails, not from
running inside Workers specifically.

