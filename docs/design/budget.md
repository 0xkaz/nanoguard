> **Status:** shipped (commit 82d65f3, 2026-05-10)

# Budget Tracking

nanoguard includes a lightweight token budget system keyed by API user.
It is designed to be simple enough for local/offline deployments.

## Storage

The current implementation uses SQLite through the `BudgetStore` trait.
That keeps the core deployment self-contained and easy to ship as a single
binary plus a local database file.

The trait exists so the storage backend can be swapped later if a deployment
needs PostgreSQL or another store.

## Keying

The budget key is derived from the OpenAI `user` field.
If `user` is missing, the proxy falls back to `default`.

This is a pragmatic identifier, not a hard authentication boundary.

## Lifecycle

Budget checks happen before the backend request is forwarded.
If the key is already at or above the configured limit, the request returns
HTTP 429 with a `budget_exceeded` error.

Usage is recorded after a successful response:

- non-streaming responses use `usage.prompt_tokens` and `usage.completion_tokens`
- streaming responses record usage when the backend emits usage metadata

If the backend does not provide usage numbers, nanoguard cannot bill that
request accurately.

## Admin API

The admin endpoints are:

- `GET /v1/admin/budget/:api_key`
- `PUT /v1/admin/budget/:api_key`
- `DELETE /v1/admin/budget/:api_key/reset`

They are protected by `ADMIN_API_KEY`.

## What This Is Not

This is not a full billing engine.

It does not currently handle:

- monthly invoice generation
- per-team rollups
- complex tenant hierarchies
- provider-specific pricing policies

The current goal is request-level enforcement and a durable usage counter.

