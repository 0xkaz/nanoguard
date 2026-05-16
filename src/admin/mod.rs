use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::{AppState, SharedState};

// ── Auth helper ───────────────────────────────────────────────────────────────

fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<(), Box<Response>> {
    let expected = match &state.config.budget.admin_api_key {
        Some(k) => k,
        None => {
            return Err(Box::new(
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "admin API is disabled"})),
                )
                    .into_response(),
            ))
        }
    };

    let provided = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");

    if provided != expected {
        return Err(Box::new(
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "invalid admin API key"})),
            )
                .into_response(),
        ));
    }

    Ok(())
}

// ── Request / response types ──────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SetLimitRequest {
    pub limit: u64,
}

#[derive(Deserialize)]
pub struct CreateClientRequest {
    /// Required user-supplied label so an operator can identify the
    /// token in lists later. The handler rejects empty / whitespace-only
    /// values with 400. `#[serde(default)]` keeps a missing field on the
    /// 400 path (with a clear error body) instead of axum's generic 422
    /// JSON-extractor error. The full secret is shown ONCE on successful
    /// creation and never persisted in plaintext.
    #[serde(default)]
    pub label: String,
    /// Optional RFC3339 timestamp at which the token expires. Null /
    /// missing = no expiry; the middleware will not reject on expiry.
    #[serde(default)]
    pub expires_at: Option<String>,
    /// Optional user id to bind the token to. Defaults to 0, the
    /// single-tenant placeholder, until the user-management work
    /// introduces real user rows.
    #[serde(default)]
    pub user_id: Option<i64>,
}

#[derive(Deserialize)]
pub struct ListClientsQuery {
    /// Required filter. Omitting `user_id` results in a 400 from the
    /// `list_clients` handler — Stage 1 does not support listing all
    /// users at once. `#[serde(default)]` keeps the field on the 400
    /// path with a clear error body instead of axum's generic 422.
    #[serde(default)]
    pub user_id: Option<i64>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// GET /v1/admin/budget/:api_key — get usage and limit
pub async fn get_budget(
    State(shared): State<SharedState>,
    headers: HeaderMap,
    Path(api_key): Path<String>,
) -> Response {
    let state = shared.load_full();
    if let Err(e) = require_admin(&state, &headers) {
        return *e;
    }

    let budget = match &state.budget {
        Some(b) => b,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "budget tracking is disabled"})),
            )
                .into_response()
        }
    };

    let usage = match budget.get_usage(&api_key).await {
        Ok(u) => u,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response()
        }
    };

    let limit = match budget.get_limit(&api_key).await {
        Ok(l) => l,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response()
        }
    };

    Json(json!({
        "api_key": api_key,
        "usage": usage,
        "limit": limit,
    }))
    .into_response()
}

/// PUT /v1/admin/budget/:api_key — set token limit
pub async fn set_budget(
    State(shared): State<SharedState>,
    headers: HeaderMap,
    Path(api_key): Path<String>,
    Json(body): Json<SetLimitRequest>,
) -> Response {
    let state = shared.load_full();
    if let Err(e) = require_admin(&state, &headers) {
        return *e;
    }

    let budget = match &state.budget {
        Some(b) => b,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "budget tracking is disabled"})),
            )
                .into_response()
        }
    };

    if let Err(e) = budget.set_limit(&api_key, body.limit).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }

    Json(json!({
        "api_key": api_key,
        "limit": body.limit,
        "status": "ok",
    }))
    .into_response()
}

/// DELETE /v1/admin/budget/:api_key/reset — reset usage counter
pub async fn reset_budget(
    State(shared): State<SharedState>,
    headers: HeaderMap,
    Path(api_key): Path<String>,
) -> Response {
    let state = shared.load_full();
    if let Err(e) = require_admin(&state, &headers) {
        return *e;
    }

    let budget = match &state.budget {
        Some(b) => b,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "budget tracking is disabled"})),
            )
                .into_response()
        }
    };

    if let Err(e) = budget.reset_usage(&api_key).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }

    Json(json!({
        "api_key": api_key,
        "status": "reset",
    }))
    .into_response()
}

// ── Client tokens ──────────────────────────────────────────────────────────

/// POST /v1/admin/clients — mint a new client token
///
/// Response includes the **full wire token** exactly once. Callers must
/// capture it on the spot; nanoguard does not persist the plaintext
/// (only `sha256(wire)` + the prefix). Subsequent `list_clients` calls
/// will only return the prefix, never the secret.
pub async fn create_client(
    State(shared): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<CreateClientRequest>,
) -> Response {
    let state = shared.load_full();
    if let Err(e) = require_admin(&state, &headers) {
        return *e;
    }

    let Some(auth) = state.client_auth.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "client_auth not enabled — set [auth].enabled or [budget].enabled"})),
        )
            .into_response();
    };

    // Validate label up front. The middleware's prefix is short enough
    // (10 chars) that operators rely on `label` to tell tokens apart in
    // the list view, so a blank label is essentially a misconfiguration
    // — reject 400 instead of persisting a NULL.
    let label_trimmed = body.label.trim();
    if label_trimmed.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "label is required and cannot be empty"})),
        )
            .into_response();
    }

    // Validate expires_at as RFC3339 at mint time. The verification path
    // is deliberately conservative on a parse failure (treats it as
    // not-expired so a corrupt timestamp does not lock anyone out), but
    // that turns a typo here into a non-expiring token — exactly the
    // false sense of security the operator was trying to avoid by
    // setting an expiry. Fail fast on the way in.
    if let Some(s) = body.expires_at.as_deref() {
        if chrono::DateTime::parse_from_rfc3339(s).is_err() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "expires_at must be RFC3339 (e.g. 2026-12-31T23:59:59Z)"
                })),
            )
                .into_response();
        }
    }

    let expires = body.expires_at.as_deref();
    let user_id = body.user_id.unwrap_or(0);

    // Prefix-collision retry. The lookup prefix is 10 chars total
    // ("ng_<env>_" + 5 from the secret) which is ~24 bits of entropy;
    // by the birthday paradox, collisions become statistically expected
    // at thousands of tokens. The DB enforces UNIQUE(prefix), so the
    // mint path retries with a freshly-generated token on a uniqueness
    // failure. 3 attempts is overkill: each attempt's collision
    // probability is bounded by (active_tokens / 24M), so 3 consecutive
    // failures at any realistic scale is vanishingly small.
    const MAX_MINT_ATTEMPTS: usize = 3;
    let mut last_err: Option<String> = None;
    let mut minted: Option<(i64, crate::client_auth::Token)> = None;
    for attempt in 1..=MAX_MINT_ATTEMPTS {
        let token = crate::client_auth::Token::generate(auth.env_marker());
        let insert_result = auth.with_conn(|conn| {
            crate::client_auth::store::insert(
                conn,
                &token.prefix,
                &token.hash,
                user_id,
                Some(label_trimmed),
                expires,
            )
        });
        match insert_result {
            Ok(id) => {
                minted = Some((id, token));
                break;
            }
            Err(e) => {
                let msg = format!("{e}");
                // SQLite UNIQUE constraint violations on `prefix` come
                // back as SqliteFailure with extended code 2067 or
                // similar. We don't pattern-match on the code (it
                // changes across versions); we look for the constraint
                // name in the message and retry. Other failures bail
                // immediately.
                let is_collision = msg.contains("UNIQUE constraint failed")
                    && msg.contains("client_tokens.prefix");
                if is_collision && attempt < MAX_MINT_ATTEMPTS {
                    tracing::warn!(
                        "admin/clients: prefix collision on attempt {}, retrying",
                        attempt
                    );
                    continue;
                }
                last_err = Some(msg);
                break;
            }
        }
    }
    let (id, token) = match minted {
        Some(pair) => pair,
        None => {
            tracing::warn!(
                "admin/clients: insert failed after {} attempts: {:?}",
                MAX_MINT_ATTEMPTS,
                last_err
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "failed to persist token"})),
            )
                .into_response();
        }
    };

    (
        StatusCode::CREATED,
        Json(json!({
            "id": id,
            "prefix": token.prefix,
            // The wire token is returned exactly once. Document this in
            // the response so a careless caller knows not to assume they
            // can re-fetch it.
            "token": token.wire,
            "warning": "Store this token now — it is not recoverable from any later API call.",
            "user_id": user_id,
            "label": label_trimmed,
            "expires_at": body.expires_at,
        })),
    )
        .into_response()
}

/// GET /v1/admin/clients?user_id=N — list tokens (prefix-only)
pub async fn list_clients(
    State(shared): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<ListClientsQuery>,
) -> Response {
    let state = shared.load_full();
    if let Err(e) = require_admin(&state, &headers) {
        return *e;
    }
    let Some(auth) = state.client_auth.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "client_auth not enabled"})),
        )
            .into_response();
    };

    // Today the store only exposes `list_for_user`. Querying the full
    // table when no user filter is supplied is a future addition; for
    // now we require an explicit user_id.
    let Some(user_id) = q.user_id else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "user_id query parameter required"})),
        )
            .into_response();
    };

    let result = auth.with_conn(|conn| crate::client_auth::store::list_for_user(conn, user_id));
    match result {
        Ok(rows) => {
            let items: Vec<_> = rows
                .into_iter()
                .map(|r| {
                    json!({
                        "id": r.id,
                        "prefix": r.prefix,
                        "user_id": r.user_id,
                        "label": r.label,
                        "created_at": r.created_at,
                        "expires_at": r.expires_at,
                        "revoked_at": r.revoked_at,
                    })
                })
                .collect();
            Json(json!({ "data": items, "object": "list" })).into_response()
        }
        Err(e) => {
            tracing::warn!("admin/clients: list failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "failed to list tokens"})),
            )
                .into_response()
        }
    }
}

/// DELETE /v1/admin/clients/:id — revoke a token
pub async fn revoke_client(
    State(shared): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    let state = shared.load_full();
    if let Err(e) = require_admin(&state, &headers) {
        return *e;
    }
    let Some(auth) = state.client_auth.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "client_auth not enabled"})),
        )
            .into_response();
    };

    let result = auth.with_conn(|conn| crate::client_auth::store::revoke(conn, id));
    match result {
        // 0 rows = already revoked OR id never existed. Either way the
        // outcome (the token is unusable) is what the caller wants, so
        // we return 200 in both cases. The `affected` field lets a
        // careful caller distinguish.
        Ok(affected) => Json(json!({
            "id": id,
            "status": "revoked",
            "affected": affected,
        }))
        .into_response(),
        Err(e) => {
            tracing::warn!("admin/clients: revoke failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "failed to revoke token"})),
            )
                .into_response()
        }
    }
}
