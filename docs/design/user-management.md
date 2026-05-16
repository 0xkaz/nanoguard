> **Status:** proposed (2026-05-16)

# User Management

`docs/design/client-auth.md` defines the token format and the
verification path. `docs/design/multi-backend-routing.md` defines
which models a user may reach. This document defines **the user
itself**: how someone logs in to the console, how their tokens
get issued, how their permissions are administered, and how the
two systems coexist with the proxy's stateless Bearer model.

Operators must not have to hand-deliver tokens. Users self-serve
their own credentials after authenticating to the console. The
console is the only place tokens come from; the proxy never issues.

## Goals

1. **Users log in to the console, not the proxy.** The proxy
   accepts Bearer tokens only. The console accepts OIDC sessions
   (or a fallback local password) and issues Bearer tokens for the
   proxy.
2. **Users self-serve their own tokens.** Create, list, revoke,
   relabel — all from the console UI, without operator action.
3. **Admins manage policy, not credentials.** Admins set per-user
   allowed_models, budget limits, and enable/disable state. They
   can force-revoke any user's tokens; they do not see the raw
   tokens themselves.
4. **OIDC first, local password fallback.** OIDC is the primary
   path for organizational deployments. A local password mode
   exists for single-operator and air-gapped deployments where no
   IdP is available.

## Non-goals

- **Multi-tenant: many organizations on one nanoguard.** One
  console instance manages one set of users. Separation across
  organizations is at the deployment level (one nanoguard per
  org), not inside the data model.
- **Fine-grained RBAC.** Two roles, `user` and `admin`. A future
  doc can split admin into roles (policy admin, user admin,
  budget admin) if needed.
- **Password recovery flows for local mode.** If the only admin
  loses their local password, they edit the SQLite file directly
  or restart with a bootstrap-admin env var. Documented; not
  productized.
- **Audit trail of admin actions.** Captured separately by the
  console-audit log defined in `docs/design/web-config-ui.md`.
  This doc references it but does not redefine it.

## Data model

Two new tables in `nanoguard.db`, alongside `client_tokens`
(defined in `client-auth.md`) and the existing budget table:

```sql
CREATE TABLE users (
  id              INTEGER PRIMARY KEY,
  username        TEXT NOT NULL UNIQUE,   -- login identifier
  display_name    TEXT,
  email           TEXT,                   -- from OIDC claims when applicable
  role            TEXT NOT NULL,          -- 'user' | 'admin'
  service_account INTEGER NOT NULL DEFAULT 0,
  disabled        INTEGER NOT NULL DEFAULT 0,
  -- Per-user policy fields
  allowed_models  TEXT NOT NULL DEFAULT '[]',   -- JSON array; see multi-backend-routing.md
  budget_limit    INTEGER,                       -- nullable; null = no per-user cap
  -- Auth metadata
  oidc_sub        TEXT UNIQUE,                   -- OIDC subject for SSO users
  password_hash   BLOB,                          -- argon2 hash for local users
  created_at      TEXT NOT NULL,
  last_login_at   TEXT
);

CREATE TABLE user_sessions (
  id           BLOB PRIMARY KEY,          -- random 32 bytes
  user_id      INTEGER NOT NULL,
  created_at   TEXT NOT NULL,
  expires_at   TEXT NOT NULL,
  last_seen_at TEXT NOT NULL,
  user_agent   TEXT,
  ip           TEXT
);
CREATE INDEX idx_user_sessions_user ON user_sessions(user_id);
```

A user record carries `oidc_sub` *or* `password_hash`, never both
populated for the same user. The console flips between modes per
user (an existing OIDC user cannot also set a local password; an
existing local-password user cannot also bind to an OIDC subject
without admin action). This avoids the "which credential just
authenticated me" ambiguity.

Sessions are short-lived (24 hours default, configurable). They
are the console's own credentials, not Bearer tokens for the
proxy. The console sends a session cookie; the proxy never sees it.

## Authentication modes

Configured per console instance:

```toml
[console.auth]
mode = "oidc"     # "oidc" | "local" | "both"

[console.auth.oidc]
issuer       = "https://idp.example.com"
client_id    = "nanoguard-console"
client_secret = "${OIDC_CLIENT_SECRET}"
redirect_uri = "https://console.example.com/oauth/callback"
# Map an OIDC claim to admin role
admin_claim  = { name = "groups", value = "nanoguard-admins" }
# Auto-create users on first login
auto_provision = true
default_role   = "user"
default_allowed_models = []   # admins grant access after first login

[console.auth.local]
# Bootstrap admin from env on first start if no users exist
bootstrap_admin = { username = "admin", password_env = "BOOTSTRAP_PASSWORD" }
allow_signup    = false       # default false; admins create accounts in local mode
```

### OIDC flow

Standard Authorization Code with PKCE.

```
1. User hits the console; no session cookie.
2. Console redirects to <issuer>/authorize?response_type=code&
   client_id=...&redirect_uri=...&scope=openid+profile+email&
   code_challenge=...&state=...
3. IdP authenticates user, redirects back to /oauth/callback?code=...
4. Console exchanges code for tokens at <issuer>/token.
5. Console verifies the ID token's signature and claims.
6. Lookup user by oidc_sub:
   - found, not disabled → log in.
   - found, disabled    → show "account disabled" page, do not login.
   - not found:
     - auto_provision = true → create user with default_role,
                                default_allowed_models, store oidc_sub.
                                Apply admin_claim mapping for role.
     - auto_provision = false → show "ask an admin to provision your
                                account" page.
7. Create a user_sessions row, set the cookie, redirect to the
   console root.
```

The OIDC ID token is verified once per login; the console never
calls the IdP per request. The session cookie is the per-request
authority.

### Local password flow

For deployments without an IdP. argon2id with default params,
salt per-user (16 bytes from a CSPRNG). No password complexity
rules baked in; admins choose to enforce them in their org. A
password is required to be at least 12 characters at the API
level (denial of weak credentials at minimum bar).

`bootstrap_admin` solves the chicken-and-egg of first-time setup:
on console start, if no users exist and the env var is set, an
admin user is created with the supplied password and the env var
is then unset from the process (logged as "bootstrap admin
provisioned; clear $BOOTSTRAP_PASSWORD").

`allow_signup = false` (default) means new local users come from
the admin UI. `allow_signup = true` exposes a `/signup` route for
self-registration; combined with `default_role = "user"` and
`default_allowed_models = []` it gives the operator a "users can
sign up but have no LLM access until I grant it" workflow.

### Both modes

When `mode = "both"`, the login page offers both options. A user
can be either an OIDC user or a local user but not both. Operators
typically use this during migration from local to OIDC.

## Session security

- **Cookie attributes**: `HttpOnly`, `Secure` (when behind HTTPS),
  `SameSite=Lax`. `Secure` is forced when the console listens on a
  non-loopback interface.
- **Rotation**: session ID is rotated on privilege escalation. The
  only escalation in the current model is "admin grants self admin
  via SQL surgery," which is out of band. Adding step-up
  authentication for admin actions is open work.
- **Idle timeout**: configurable, default 24 hours from
  `last_seen_at`. Sessions older than `expires_at` are evicted by
  a background sweep every 5 minutes.
- **CSRF**: every state-changing form submits a double-submit token
  bound to the session ID. The token rotates on every successful
  mutation, as already noted in `web-config-ui.md`.
- **Brute force**: local-password login is rate-limited per
  username + per source IP. After 10 failures in 15 minutes, the
  account is locked for 15 minutes (visible to admin in the user
  detail view). OIDC inherits the IdP's brute-force protection.

## User self-service surface

Once logged in, a user can:

### Token management

- **List own tokens**: prefix, label, created_at, last_used_at,
  expires_at, revoked_at. Sortable, filterable.
- **Create a token**: form fields `label` (required), `expires_at`
  (optional). On submit, the full secret is shown exactly once in
  a modal with a copy-to-clipboard button. Closing the modal
  clears the secret from the DOM. The token row in the list
  immediately shows the prefix; the secret is never recoverable.
- **Revoke a token**: a confirmation step, then sets `revoked_at`.
  The proxy's token cache is invalidated via broadcast.
- **Relabel a token**: edit the `label` field. Does not affect the
  hash or the prefix.

### Budget and usage

- **Budget overview**: own usage / limit, per-token breakdown,
  per-model breakdown for the last N days. Read-only.
- **Audit slice**: own request history. Server-side filter on
  `user_id` against the audit log. Same JSONL the admin sees but
  scoped.

### Profile

- **Display name / email**: editable for local users; read-only
  for OIDC users (the IdP is authoritative).
- **Change password**: local mode only. Requires current password.
- **Sessions**: list active sessions; revoke one or all-but-current.

## Admin surface

The admin role unlocks additional pages:

### User management

- **List all users**: filter by role, by disabled state, by
  service account.
- **Create user**: local mode → username + temporary password,
  flagged "password change required at first login." OIDC mode →
  manual entry of `oidc_sub` (rare; usually auto-provisioned).
- **Edit user**: change role, set/clear disabled, change
  `allowed_models`, change `budget_limit`.
- **Force-revoke all tokens for a user**: bulk operation. Useful
  when a user leaves the organization or a credential is
  suspected compromised.
- **Delete user**: soft delete (sets `disabled = 1`) by default.
  Hard delete requires a typed confirmation and removes the user
  row plus cascades to tokens.

### Per-user policy editor

A single form per user that maps to the policy fields:

- `allowed_models`: model picker, populated from the current
  `[routing]` table. Console prevents saving a model that has no
  route (the misconfig case in `multi-backend-routing.md` step 5).
- `budget_limit`: integer in tokens, nullable.
- (Future) per-user PII overrides, per-user spotlight overrides:
  reserved space in the form; not in scope here.

### Auditability of admin actions

Every admin action writes a record to `console-audit.jsonl` (the
console's own audit log, separate from the proxy's audit log):

```json
{
  "timestamp": "2026-05-16T14:32:00Z",
  "actor":     "admin-alice",
  "action":    "user.set_allowed_models",
  "target":    "user:carol",
  "before":    ["gpt-4o-mini"],
  "after":     ["gpt-4o-mini", "claude-sonnet-4-6"]
}
```

The proxy never reads this file; it exists for the operator to
answer "who granted this permission and when."

## Interaction with the proxy

The proxy stays stateless with respect to users. The token
verification path defined in `client-auth.md` performs an in-
memory lookup keyed by token prefix; the lookup returns a
`ClientView` that already carries the user's `allowed_models` and
`budget_limit`. The proxy does not consult the `users` table on
the request path.

The console keeps the proxy's in-memory cache in sync:

- **User role change**: invalidate every cached `ClientView` for
  the user (broadcast carries `user_id`).
- **User disabled**: same. Subsequent verifications return 401.
- **`allowed_models` change**: same. The next request sees the new
  list.
- **Token revoke**: invalidate that token's `ClientView`.

The cache TTL (default 60s, from `client-auth.md`) is the fallback
when a broadcast is lost. Operators who want stricter "revocation
must be effective within N seconds" can reduce the TTL.

## Configuration summary

The full set, combining this doc and the auth/routing docs:

```toml
[auth]
enabled        = true
env_marker     = "p"
cache_capacity = 10000
cache_ttl_secs = 60
require_https  = true
admin_api_key  = "${ADMIN_API_KEY}"

[console]
listen      = "127.0.0.1:8081"
session_secret = "${CONSOLE_SESSION_SECRET}"   # signing key for session cookies
session_ttl_hours = 24

[console.auth]
mode = "oidc"

[console.auth.oidc]
issuer       = "https://idp.example.com"
client_id    = "nanoguard-console"
client_secret = "${OIDC_CLIENT_SECRET}"
redirect_uri = "https://console.example.com/oauth/callback"
admin_claim  = { name = "groups", value = "nanoguard-admins" }
auto_provision = true
default_role   = "user"
default_allowed_models = []

[console.auth.local]
allow_signup = false
# bootstrap_admin only honored when no users exist in the DB
bootstrap_admin = { username = "admin", password_env = "BOOTSTRAP_PASSWORD" }
```

## Threat model

- **Session theft**: HttpOnly + Secure + SameSite=Lax + rotation
  on privilege change. TLS termination required for non-loopback
  deployment.
- **OIDC token theft**: the ID token is exchanged once; the
  session cookie is the long-lived credential after that.
  Compromise of the cookie compromises one session, not the IdP
  account.
- **Password database leak**: argon2id with per-user salt. Cracking
  cost is bounded by the algorithm's parameters. Operators are
  advised to use OIDC where available.
- **Disabled user with live tokens**: handled by cache
  invalidation. Worst-case window is the cache TTL.
- **Admin compromise**: an admin can grant themselves any
  permission, edit policies, revoke any token. The mitigation is
  the console-audit log + operational hygiene (named admins, MFA
  at the IdP layer, no shared admin accounts). The proxy itself
  has no further defense.
- **OIDC misconfiguration**: a misconfigured `admin_claim`
  matcher that accepts too broadly is a privilege escalation. The
  console validates the matcher syntax on save and tests it
  against the currently-authenticated admin's claims before
  applying.

## Rollout

This work is gated on `client-auth.md` landing first.

1. **Phase 1**: data model (tables + migrations) + admin API for
   user CRUD. No UI yet. Operators can poke users in via curl
   and test the proxy auth path.
2. **Phase 2**: local-password login flow + console pages for own
   tokens + own budget. Bootstrap admin from env.
3. **Phase 3**: admin UI for user CRUD, per-user policy editor,
   force-revoke. Console-audit log surface.
4. **Phase 4**: OIDC flow. Auto-provisioning. `admin_claim`
   matcher.
5. **Phase 5**: hardening — CSRF, brute-force limits, session
   rotation polish, password strength checks.

Each phase is independently useful: Phase 2 alone is enough for a
single-operator + small team deployment; Phase 4 is what
organizations need.

## Open questions

- **Service-account UX**: should the console show service-account
  users in a separate list, with no login (only token issuance by
  an admin)? Direction: yes, separate list, but same underlying
  table with `service_account = true`. The "login" surface for
  these users is just "an admin issues tokens against the
  account." Defer the UI shape to web-config-ui.md.
- **Hierarchical permissions / teams**: a user belongs to a team,
  team has policies, user inherits. Powerful but a layer above
  what is needed in v1. Punt.
- **MFA for local-password admins**: TOTP would be a small,
  well-scoped addition. Out of scope for v1; tracked.
- **Token scopes** (per-token "this token can only read budget"):
  conflated with allowed_models today. Generalizing to arbitrary
  scopes (`budget:read`, `audit:read`, `chat`) is a future
  refinement.
- **External user directory sync** (SCIM, LDAP): out of scope.
  OIDC's auto-provisioning + `admin_claim` matcher cover most
  needs; full directory sync is a v2+ concern.
