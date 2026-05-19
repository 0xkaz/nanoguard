> **Status:** partial (commit cdc795c, 2026-05-16 — Stage 1 ships token format, storage, verification middleware, admin CRUD, and cache; the integration glue with budget / audit / `users` / config / `last_used_at` / shadow mode is still open. See "Still open" below.)

# Client Authentication

**Stage 1 of the rollout is shipped:** token format, SQLite storage,
the axum verification middleware, the admin CRUD endpoints
(`/v1/admin/clients` POST/GET/DELETE), an opt-in `[auth].enabled`
flag, and the in-memory verification cache with TTL + explicit
invalidation on revoke. Existing deployments are not affected
because the flag defaults to `false`.

**Still open** (keeps the doc at `partial` rather than `shipped`).
Sections below tagged *Stage 2* describe a target state that is
**not wired** today; the verification middleware exists but does
not yet feed budget, audit, or PII off it:

- ~~Budget integration is not wired.~~ **Shipped** (2026-05-18,
  scenario 39). `ClientView` carries a `budget_key` slot set to
  `token:<id>`; `/v1/chat/completions` uses it when the verifier has
  attached a view. The legacy body-`user` bucket is preserved when
  `[auth].enabled = false`. The `user:<id>` aggregate admin form
  remains a follow-up (depends on the `users`-table slice).
- **Audit `user_id` / `token_id` not added.** `AuditEntry` has no
  `user_id` or `token_id` field; the `api_key` column is still
  populated from the request body, not from `ClientView`.
- **`[auth].admin_api_key` not implemented.** `AuthConfig` has no
  `admin_api_key` field. `require_admin` continues to read
  `[budget].admin_api_key`. The migration described under
  Configuration has not started.
- **`last_used_at` flush not wired.** The column exists in the
  schema but no `UPDATE client_tokens SET last_used_at` path
  runs from the verifier, and there is no background flush task.
  The value is always `NULL` in Stage 1.
- **`users` table integration.** `user_id` defaults to 0 today as a
  single-tenant placeholder; the "User disabled" failure mode lands
  with the user-management work.
- **Per-token `pii_overrides`.** No PII action consults a per-token
  override yet. The `ClientView` slot for it has not been added.
- **`allowed_models` on `ClientView`.** Reserved for the multi-
  backend routing work; not on `ClientView` today.
- **Shadow mode (Stage 2 of the rollout).** `[auth].enabled` is a
  strict bool today; the `"shadow"` string is not yet recognized.

Today the proxy endpoints (`/v1/chat/completions`, `/v1/messages`,
`/v1/models`, `/health`) accept any caller on the network. Only
`/v1/admin/budget/*` is gated, by `ADMIN_API_KEY`. That gap is the
reason nanoguard cannot truthfully claim to be a policy boundary in
a multi-user deployment: a determined client can simply bypass it
and call the upstream LLM directly with their own key. Once
nanoguard holds the upstream credentials and forces every call
through itself, authentication on the proxy side becomes the load-
bearing link.

This document defines a **client token** model. A client token is a
Bearer credential issued by nanoguard for use against the proxy.
It is distinct from any upstream LLM key (which the proxy holds and
never returns to callers). Issuance, revocation, and the user-
facing UI for both live in `docs/design/user-management.md`; this
doc defines only the on-the-wire shape and the verification path.

## Goals

1. **Every proxy request is attributable.** The audit log's
   `api_key` field stops being self-asserted and becomes a verified
   token identifier.
2. **Tokens can be revoked.** A compromised token must be killable
   without restarting the proxy and without rotating the upstream
   LLM key.
3. **Tokens carry the per-client policy axis.** The same token
   that authenticates a request resolves the budget bucket, the
   allowed-models list (see `docs/design/multi-backend-routing.md`),
   and any per-user PII override.
4. **Verification stays on the hot path.** Token check must be
   O(1) and must not require disk I/O per request. Caching a
   compact "client view" in memory is required.

## Non-goals

- **Replacing the existing admin Bearer.** `ADMIN_API_KEY` remains
  a single shared secret for the budget admin API. The client-
  token system does not subsume it; admin and client are separate
  authorities. (A unified RBAC story is a future doc.)
- **Defining the user concept or login flow.** Users, OIDC, local
  passwords, and the issuance UI live in
  `docs/design/user-management.md`. This doc only requires that
  *some* issuer writes tokens into the store described below.
- **Federated tokens / SSO assertions on the proxy endpoint.**
  The proxy accepts only nanoguard-issued Bearer tokens. SSO is for
  the console, which issues tokens after SSO completes.
- **Per-request signing (HMAC, mTLS).** Bearer over TLS is the
  baseline. Stronger schemes are out of scope here.

## Token format

```
ng_<env>_<rand24>
```

- `ng_` — fixed prefix; greppable in logs and source code (so a
  token leaked into a commit is detectable by secret scanners).
- `<env>` — single character: `p` (production) or `t` (test /
  dev). Lets ops tell at a glance which environment a leaked
  token belongs to. Configurable; defaults to `p`.
- `<rand24>` — 24 base32 characters (lowercase, Crockford alphabet
  minus ambiguous chars). ~120 bits of entropy. Sufficient for the
  threat model (online guessing against a rate-limited endpoint).

Example: `ng_p_a3k7m2x9b8q4r6t5w1n8h2y0`

The token is **shown to the user exactly once**, at creation time.
Only a SHA-256 hash and a short prefix (`ng_p_a3k7…`) are persisted.
The prefix is what surfaces in the UI and in audit entries for
human identification; the hash is what verification compares
against.

Why prefix+hash rather than the bare token in DB:

- A DB dump does not leak live credentials.
- Operators can still talk about "the `a3k7…` token" without
  needing the secret.
- Verification is a single hash + lookup.

## Storage

Token records live in the existing `nanoguard.db` SQLite database
(alongside budget). A new table:

```sql
CREATE TABLE client_tokens (
  id           INTEGER PRIMARY KEY,
  prefix       TEXT NOT NULL UNIQUE,   -- "ng_p_a3k7"
  hash         BLOB NOT NULL,          -- sha256(full_token), 32 bytes
  user_id      INTEGER NOT NULL,       -- FK to users (see user-management.md)
  label        TEXT,                   -- user-supplied "what is this for"
  created_at   TEXT NOT NULL,
  expires_at   TEXT,                   -- nullable; null = no expiry
  revoked_at   TEXT,                   -- nullable; set on revoke
  last_used_at TEXT                    -- soft-updated, see below
);
CREATE INDEX idx_client_tokens_prefix ON client_tokens(prefix);
CREATE INDEX idx_client_tokens_user   ON client_tokens(user_id);
```

`prefix` is the lookup key on the hot path. The full hash is
compared in constant time after the row is found to defeat timing
side-channels (`subtle::ConstantTimeEq` via the `subtle` crate).

`last_used_at` is intended to be updated on a best-effort basis:
not every request writes; the proxy buffers and flushes every N
seconds (default 30) on a background task. Losing a few seconds
of "last used" precision on crash is acceptable; making every
request do a SQLite UPDATE is not.

> **Stage 1 — not wired.** The schema carries the column and the
> store/admin/console read it, but neither the verification
> middleware nor any background task issues `UPDATE client_tokens
> SET last_used_at`. The value is always `NULL` until Stage 2
> lands.

## Verification path

On every request to `/v1/chat/completions`, `/v1/messages`,
`/v1/models`:

```
1. Read Authorization header.
   No header / wrong scheme → 401, body: {"error":"missing bearer"}
2. Split "Bearer ng_p_a3k7m2x9b8q4r6t5w1n8h2y0" → token string.
3. Extract prefix (first 10 chars including ng_).
   Doesn't start with ng_ / wrong length → 401, body: {"error":"malformed token"}
4. Look up the client view by prefix in the in-memory cache (see Caching).
   Not found → 401, body: {"error":"unknown token"}
5. Verify sha256(token) == stored hash, constant time.
   Mismatch → 401, body: {"error":"unknown token"}
   (Same body as step 4 — do not reveal whether the prefix existed.)
6. Check expires_at > now and revoked_at IS NULL.
   Expired / revoked → 401, body: {"error":"token expired"} or {"error":"token revoked"}
7. Attach the resolved ClientView to the request extensions.
   Downstream handlers (budget, allowlist, audit, redactor)
   consume it from there.
8. Asynchronously bump last_used_at in the flush buffer.
```

`/health` is **not** authenticated. It returns the proxy's own
liveness, not any policy state, and load balancers need it
unauthenticated.

`/v1/models` returns only the models the authenticated client is
allowed to use (see `multi-backend-routing.md`). Unauthenticated
callers get 401.

### ClientView

The in-memory shape attached to each request as of 2026-05-18
(Stage 2 slice 1 shipped):

```rust
struct ClientView {
    token_id:        i64,
    token_prefix:    String,         // "ng_p_a3k7"
    user_id:         i64,
    label:           Option<String>,
    budget_key:      String,         // "token:<token_id>" — see Budget integration
}
```

The fields below remain part of the longer-term design but are **not
present today**:

- `allowed_models: AllowedModels` — Stage 2, lands with
  `multi-backend-routing.md`.
- `pii_overrides: Option<Arc<PiiOverrides>>` — future, see Open
  questions.

Cheap to clone. Created at verification time, dropped at request
end. Never serialized.

## Caching

The hot path must not hit SQLite per request. A read-through cache
keyed by token prefix sits inside `ClientAuth`:

- **Capacity**: bounded by `[auth].cache_capacity` (default 10_000
  entries). On insert into a full cache, expired entries are
  preferred for eviction; if every slot is still fresh, the entry
  with the smallest `last_used` counter (least recently used) is
  dropped.
- **TTL**: `[auth].cache_ttl_secs` (default 60). After TTL, the next
  verification re-reads from SQLite. This bounds the maximum
  staleness window for revocations.
- **Negative caching**: not done. A lookup that misses in SQLite
  does not populate the cache. Two reasons: (a) a flood of bogus
  prefixes must not be able to grow the cache; (b) a newly-minted
  token has to be visible on the very next request, which
  caching a `None` would defeat.
- **Invalidation**: explicit, in-process. The admin revoke handler
  resolves the token's prefix and calls `auth.invalidate_cached`
  before returning. The next request for that prefix re-reads
  SQLite and observes `revoked_at`. The TTL is the fallback for
  any future revocation source that does not call `invalidate_cached`
  (e.g., direct SQLite edits).

A direct in-process call beats a tokio broadcast channel here
because the revoke admin handler and the verification middleware
both run in the same process against the same `Arc<TokenCache>`.
A broadcast layer would add complexity without removing any race:
the channel reader sits on the same task pool as the verifier.

The cache is in-memory only. Process restart clears it; the next
batch of requests warms it from SQLite. That is the intended
behavior — there is no persistent cache state to lose.

## Budget integration

> **Status: partial.** As of 2026-05-18 the `token:<id>` budget
> bucket is shipped: `ClientView` carries `budget_key`, and
> `/v1/chat/completions` consults it whenever the verifier has
> attached a view. The legacy body-`user` bucket is preserved on
> deployments with `[auth].enabled = false`. The `user:<id>`
> aggregate admin form is **not yet implemented** and depends on
> the `users`-table slice. `/v1/messages` (Anthropic) also still
> bypasses budget accounting entirely — that is the next slice.

The existing `[budget]` machinery uses the OpenAI `user` field
from the request body as the budget key, falling back to
`"default"`. That is a workable identifier today because there is
no other one available; it becomes redundant once tokens exist.

The new contract:

- `ClientView.budget_key = format!("token:{}", token_id)`. The
  budget bucket is per-token. A user with three tokens has three
  separate buckets, which is correct: a runaway script under one
  token does not deplete the user's interactive token.
- Per-**user** aggregated views are still possible by joining
  `client_tokens.user_id` at read time. The existing admin
  endpoint `/v1/admin/budget/:api_key` accepts the
  `token:<id>` form. A `user:<id>` form is added for aggregate
  queries.
- The OpenAI `user` field in the request body is preserved and
  forwarded to the upstream backend unchanged. It is no longer
  consulted for budget accounting.

This is a behavior change for existing deployments that rely on
the `user`-field-as-budget-key convention. The migration is
covered in the rollout section below.

## Audit integration

> **Stage 2 — not yet applied.** `AuditEntry` (`src/audit/mod.rs`)
> has no `user_id` or `token_id` field today, and the `api_key`
> column is still sourced from `extract_api_key` against the
> request body. The "After" column below is the proposed target
> state, not what the audit log currently emits.

The audit log's existing `api_key` field becomes the verified token
prefix (e.g. `ng_p_a3k7`), not the OpenAI `user` field. The
mapping is:

| Audit field    | Before (current Stage 1)           | After (Stage 2, proposed)         |
|----------------|------------------------------------|-----------------------------------|
| `api_key`      | request body's `user` or `default` | `ClientView.token_prefix`         |
| (new) `user_id`| absent                             | `ClientView.user_id` as a string  |
| (new) `token_id`| absent                            | `ClientView.token_id` as a string |

`user_id` and `token_id` are added to support joining audit lines
against the console's user table. They are omitted from the JSON
when an unauthenticated endpoint (none currently emit audit
entries) writes a line.

This is also a backward-compatible JSON change: consumers that
read `api_key` keep working; new consumers can filter by
`user_id` to follow one user across token rotations.

## Failure modes

| Condition                                  | Response                                                    |
|--------------------------------------------|-------------------------------------------------------------|
| No `Authorization` header                  | 401 + `WWW-Authenticate: Bearer realm="nanoguard"`          |
| Authorization scheme not `Bearer`          | 401 (same)                                                  |
| Token prefix not in cache or DB            | 401 (uniform body — see verification step 4)                |
| Token found, hash mismatch                 | 401 (uniform body, constant time)                           |
| Token expired                              | 401, body identifies "expired" (distinct from "unknown")    |
| Token revoked                              | 401, body identifies "revoked"                              |
| User disabled (Stage 2, with `users` table)| 401, body identifies "user disabled" — not implemented today, no `users` table to read from |
| Cache lookup successful, hot path proceeds | 200 / normal proxy flow                                     |

The split between "unknown" (uniform) and "expired/revoked/disabled"
(distinct) is intentional. Distinguishing among the latter three
helps legitimate users diagnose; collapsing all four would force
them to ask an operator. The uniform branch protects against
enumeration of valid prefixes.

Constant-time comparison applies to step 5 (hash check) only. The
other failure modes are intrinsically distinguishable from the
client's perspective (different status messages), and there is no
secret to leak from a timing difference between, say, an expired
token and a revoked one.

All 401 responses include a `WWW-Authenticate: Bearer …` header,
not just the "no Authorization header" case — the table above
lists the realm form only on the first row to avoid repetition.
The malformed / unknown / expired / revoked branches add
`error="invalid_token"` per RFC 6750.

## Configuration

A new section in `nanoguard.toml`:

```toml
[auth]
enabled = false                   # Stage 1 default; flips to true at Stage 3
env_marker = "p"                  # 'p' production, 't' test/dev
cache_capacity = 10000
cache_ttl_secs = 60
require_https = false             # see Transport below
```

> **Stage 2 — `[auth].admin_api_key` not yet implemented.**
> Stage 1 `AuthConfig` has no `admin_api_key` field; `require_admin`
> in `src/admin/mod.rs` still reads `state.config.budget.admin_api_key`.
> The plan, when this migrates, is to add an `admin_api_key` line to
> the block above, preserve `[budget].admin_api_key` for one release
> with a deprecation warning, then remove it. `[auth].admin_api_key`
> is the right long-term home because admin authentication is a
> peer of client authentication, not a sub-feature of budget.

### Transport

Bearer tokens are credentials. nanoguard does not terminate TLS
itself by default — the deployment model assumes a reverse proxy
(Caddy / nginx / Cloudflare / a managed LB) in front. When
`[auth].require_https = true`, the proxy rejects requests where
the `X-Forwarded-Proto` header is not `https`. The check is
unconditional when enabled: there is no loopback exemption in
Stage 1 because the middleware does not have `ConnectInfo`
plumbing to distinguish loopback from non-loopback clients.
Operators running on loopback should leave `require_https = false`.

This is opt-in because operators who run nanoguard behind a
local proxy that strips the header still need it to work. The
recommendation in the docs will be "set `require_https = true`
unless you know exactly what is in front of nanoguard."

## Rollout

A breaking change like "every request now needs a Bearer token"
cannot land in one commit on an existing deployment without
downtime. The plan:

1. **Stage 1 — opt-in.** `[auth] enabled = false` default. Code
   path exists; nothing enforces. The console and the admin API
   gain the issuance + revocation surface.
2. **Stage 2 — shadow mode.** `[auth] enabled = "shadow"`. Every
   request is verified, audit logs gain `user_id` / `token_id`,
   but unauthenticated requests still pass through with
   `api_key = "default"` and a `warn` log line. Operators verify
   their clients have moved over.
3. **Stage 3 — enforcement.** `[auth] enabled = true`. Unauth'd
   requests get 401. Default in `nanoguard.toml.example` flips at
   this point.

The shadow phase is what the existing `[input] shadow` mechanism
does for keyword rules; reusing the pattern keeps the operator
experience consistent.

## Threat model and limits

- **Token leakage** is the dominant risk. Mitigations: prefix-only
  display, hash-at-rest, secret scanner-friendly prefix, transport
  TLS, audit log records every use, rate-limiting (separate doc).
- **Token theft via XSS in the console** is the second risk. The
  console never exposes the full token after creation; the
  one-time display is in a `<dialog>` that requires an explicit
  "copy to clipboard" click and clears the DOM node after.
- **Online guessing of prefixes** is bounded by rate limiting +
  the 120-bit entropy in the secret portion. A naive 100 req/s
  attacker against a single prefix needs ~10^28 seconds to find
  the matching secret.
- **DB exfiltration** does not yield usable tokens (only prefix +
  hash). It does yield a list of token labels and user mappings,
  which is itself sensitive — operators should protect the SQLite
  file with filesystem permissions and disk encryption.
- **Admin token compromise** still yields full budget control. The
  admin key is single-shared-secret; multi-admin with named
  accounts is deferred until the user-management work lands.
- **No protection against the upstream LLM seeing leaked content
  via a compromised client token.** A valid token = a valid
  caller. Defense for that scenario is upstream content controls,
  not authentication.

## Open questions

- **Token expiry default**: leave as `NULL` (no expiry) and rely
  on revocation, or default to e.g. 90 days? Direction: NULL
  default, surface expiry as a per-token opt-in in the UI. Long-
  lived tokens are the common case; forcing expiry creates a
  rotation toil that operators will route around.
- **Per-token PII overrides**: punted to a future doc. A
  `ClientView.pii_overrides` slot is intended to be added later so
  the request pipeline can grow into it; it is not on `ClientView`
  in Stage 1.
- **Token rotation API**: should there be a "rotate token N"
  primitive that issues a new token, ties it to the same
  user/labels/quotas, and revokes the old one after a grace
  period? Useful, but additive over the basic create/revoke
  surface; defer to the user-management doc.
- **Service-account tokens vs human-user tokens**: the schema
  treats them identically (a user record can be marked
  `service_account = true`). Whether the console exposes
  different UIs for the two is a UX question for the web-config
  doc.
