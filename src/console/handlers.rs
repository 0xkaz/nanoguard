//! HTTP handlers for the nanoguard console.

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use std::collections::HashMap;
use std::sync::Arc;

use crate::client_auth::{store as token_store, Token};

use super::{
    audit::MutationRecord,
    auth::{
        build_logout_cookie, build_session_cookie, encode_csrf_token, generate_csrf_token,
        verify_password, CurrentUser, MutatingUser, CSRF_NEXT_HEADER,
    },
    db::{self, User},
    ConsoleState,
};

/// Rotate the CSRF token on a session row and return the new wire-encoded
/// value. Logs a warning if the rotation fails (which would only happen if
/// the session was concurrently deleted) — the mutation itself has already
/// succeeded, so a failed rotation does not change the response status, but
/// the client will fall back to its prior token and re-authenticate on the
/// next mutation.
fn rotate_csrf(state: &ConsoleState, session_id: &[u8]) -> Option<String> {
    let new_raw = generate_csrf_token();
    match state
        .db
        .with_conn(|conn| db::rotate_csrf_token(conn, session_id, &new_raw))
    {
        Ok(n) if n > 0 => Some(encode_csrf_token(&new_raw)),
        Ok(_) => {
            tracing::warn!("csrf: rotate skipped (session no longer exists)");
            None
        }
        Err(e) => {
            tracing::warn!("csrf: rotate failed: {}", e);
            None
        }
    }
}

/// Build a [`HeaderMap`] carrying the rotated CSRF token so the JS client
/// can refresh its cached value. Returns an empty map when rotation
/// produced no value (i.e. the session row had already been deleted).
fn csrf_next_headers(token: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(t) = token {
        if let Ok(value) = axum::http::HeaderValue::from_str(t) {
            headers.insert(axum::http::HeaderName::from_static(CSRF_NEXT_HEADER), value);
        }
    }
    headers
}

// ── Request / response types ────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Deserialize)]
pub struct CreateTokenRequest {
    pub label: String,
    #[serde(default)]
    pub expires_at: Option<String>,
}

#[derive(Deserialize)]
pub struct CreateUserRequest {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub role: Option<String>,
}

#[derive(Deserialize)]
pub struct ResetPasswordRequest {
    pub password: String,
}

#[derive(Deserialize)]
pub struct UpdateUserRequest {
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub disabled: Option<bool>,
    #[serde(default)]
    pub allowed_models: Option<String>,
    #[serde(default)]
    pub budget_limit: Option<Option<i64>>,
}

#[derive(Deserialize, Default)]
pub struct AuditQuery {
    #[serde(default)]
    pub verdict: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Deserialize, Default)]
pub struct ConsoleAuditQuery {
    /// Filter by action kind (e.g. "login", "user_update", "edit"). For
    /// backward compat the `verdict` query param is also accepted with the
    /// same meaning.
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub verdict: Option<String>,
    /// Filter by `actor` field (the admin or user who performed the action).
    #[serde(default)]
    pub actor: Option<String>,
    /// Filter by `target` field (the user / file the action was performed on).
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Serialize)]
pub struct UserResponse {
    pub id: i64,
    pub username: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub role: String,
    pub service_account: bool,
    pub disabled: bool,
    pub allowed_models: String,
    pub budget_limit: Option<i64>,
    pub locked_until: Option<String>,
    pub created_at: String,
    pub last_login_at: Option<String>,
}

impl From<User> for UserResponse {
    fn from(u: User) -> Self {
        Self {
            id: u.id,
            username: u.username,
            display_name: u.display_name,
            email: u.email,
            role: u.role,
            service_account: u.service_account,
            disabled: u.disabled,
            allowed_models: u.allowed_models,
            budget_limit: u.budget_limit,
            locked_until: u.locked_until,
            created_at: u.created_at,
            last_login_at: u.last_login_at,
        }
    }
}

// ── Auth helpers ────────────────────────────────────────────────────────────

fn require_admin(user: &User) -> Result<(), Box<Response>> {
    if user.role != "admin" {
        return Err(Box::new(
            (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "admin required"})),
            )
                .into_response(),
        ));
    }
    Ok(())
}

fn session_cookie(state: &ConsoleState, session_id: &[u8]) -> cookie::Cookie<'static> {
    build_session_cookie(
        session_id,
        &state.config.console.session_secret,
        state.secure_cookie,
    )
}

fn logout_cookie(state: &ConsoleState) -> cookie::Cookie<'static> {
    build_logout_cookie(&state.config.console.session_secret, state.secure_cookie)
}

// ── HTML page ───────────────────────────────────────────────────────────────

/// Serve the single-page app HTML.
pub async fn index() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        super::assets::INDEX_HTML,
    )
}

/// Serve embedded CSS.
pub async fn styles() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/css")],
        super::assets::STYLES_CSS,
    )
}

/// Serve embedded JS.
pub async fn app_js() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "application/javascript")],
        super::assets::APP_JS,
    )
}

// ── API: Auth ───────────────────────────────────────────────────────────────

pub async fn api_login(
    State(state): State<Arc<ConsoleState>>,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> Response {
    if state.config.console.session_secret.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "console session_secret not configured"})),
        )
            .into_response();
    }

    let ip = headers
        .get("x-forwarded-for")
        .or_else(|| headers.get("x-real-ip"))
        .and_then(|v| v.to_str().ok());

    // Per-IP rate limiting - check early to block repeated attempts
    let max_attempts = state.config.console.max_login_attempts;
    let lockout_minutes = state.config.console.lockout_duration_minutes;
    if let Some(ip_str) = ip {
        let ip_count = state
            .db
            .with_conn(|conn| db::count_recent_login_attempts_by_ip(conn, ip_str, lockout_minutes))
            .unwrap_or(0);
        if ip_count >= max_attempts {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({"error": "too many requests from this IP"})),
            )
                .into_response();
        }
    }

    let user = match state
        .db
        .with_conn(|conn| db::user_by_username(conn, &body.username))
    {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!("login: db error: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal error"})),
            )
                .into_response();
        }
    };

    let Some(user) = user else {
        // Record failed attempt even when user does not exist (username enumeration
        // defense: always return the same error and timing).
        let _ = state
            .db
            .with_conn(|conn| db::record_login_attempt(conn, &body.username, ip));
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid credentials"})),
        )
            .into_response();
    };

    // Check lockout
    if let Some(ref locked_until) = user.locked_until {
        let locked = chrono::DateTime::parse_from_rfc3339(locked_until)
            .map(|dt| chrono::Utc::now() < dt)
            .unwrap_or(false);
        if locked {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({"error": "account locked"})),
            )
                .into_response();
        }
    }

    if user.disabled {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "account disabled"})),
        )
            .into_response();
    }

    let Some(ref hash) = user.password_hash else {
        let _ = state
            .db
            .with_conn(|conn| db::record_login_attempt(conn, &user.username, ip));
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid credentials"})),
        )
            .into_response();
    };

    if !verify_password(&body.password, hash) {
        let _ = state
            .db
            .with_conn(|conn| db::record_login_attempt(conn, &user.username, ip));
        // Check if we should lock the account
        let max_attempts = state.config.console.max_login_attempts;
        let lockout_minutes = state.config.console.lockout_duration_minutes;
        let should_lock = state
            .db
            .with_conn(|conn| {
                let count = db::count_recent_login_attempts(conn, &user.username, lockout_minutes)?;
                Ok(count >= max_attempts)
            })
            .unwrap_or(false);
        if should_lock {
            let until =
                (chrono::Utc::now() + chrono::Duration::minutes(lockout_minutes)).to_rfc3339();
            let _ = state
                .db
                .with_conn(|conn| db::set_locked_until(conn, user.id, Some(&until)));
            tracing::warn!(
                "login: account {} locked until {} after {} failed attempts",
                user.username,
                until,
                max_attempts
            );
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({"error": "account locked"})),
            )
                .into_response();
        }
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid credentials"})),
        )
            .into_response();
    }

    // Clear failed login attempts on success.
    let _ = state
        .db
        .with_conn(|conn| db::clear_login_attempts(conn, &user.username));
    // Also clear any stale lockout.
    let _ = state
        .db
        .with_conn(|conn| db::set_locked_until(conn, user.id, None));

    if let Err(e) = state
        .db
        .with_conn(|conn| db::update_last_login(conn, user.id))
    {
        tracing::warn!("login: failed to update last_login: {}", e);
    }

    let session_id = super::auth::generate_session_id();
    let csrf_raw = generate_csrf_token();
    let ttl_hours = state.config.console.session_ttl_hours;
    let expires = chrono::Utc::now() + chrono::Duration::hours(ttl_hours);
    let user_agent = headers.get("user-agent").and_then(|v| v.to_str().ok());

    if let Err(e) = state.db.with_conn(|conn| {
        db::create_session(
            conn,
            &session_id,
            user.id,
            &expires.to_rfc3339(),
            user_agent,
            ip,
            &csrf_raw,
        )
    }) {
        tracing::warn!("login: failed to create session: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "session creation failed"})),
        )
            .into_response();
    }

    if let Some(ref log) = state.audit_log {
        log.record_mutation(
            &MutationRecord::new(
                &user.username,
                user.id,
                "login",
                format!("{} logged in", user.username),
            )
            .with_target(user.username.clone()),
        );
    }

    let cookie = session_cookie(&state, &session_id);
    let csrf_wire = encode_csrf_token(&csrf_raw);
    (
        StatusCode::OK,
        [(axum::http::header::SET_COOKIE, cookie.to_string())],
        Json(json!({
            "ok": true,
            "user": UserResponse::from(user),
            "csrf_token": csrf_wire,
        })),
    )
        .into_response()
}

pub async fn api_logout(
    State(state): State<Arc<ConsoleState>>,
    MutatingUser { user, session_id }: MutatingUser,
) -> Response {
    let _ = state
        .db
        .with_conn(|conn| db::delete_user_sessions(conn, user.id));
    if let Some(ref log) = state.audit_log {
        log.record_mutation(
            &MutationRecord::new(
                &user.username,
                user.id,
                "logout",
                format!("{} logged out", user.username),
            )
            .with_target(user.username.clone()),
        );
    }

    let cookie = logout_cookie(&state);
    let next_csrf = rotate_csrf(&state, &session_id);
    let headers = csrf_next_headers(next_csrf.as_deref());
    let mut response_headers = HeaderMap::new();
    if let Ok(cookie_value) = axum::http::HeaderValue::from_str(&cookie.to_string()) {
        response_headers.insert(axum::http::header::SET_COOKIE, cookie_value);
    }
    for (k, v) in headers {
        if let Some(k) = k {
            response_headers.insert(k, v);
        }
    }
    (StatusCode::OK, response_headers, Json(json!({"ok": true}))).into_response()
}

pub async fn api_me(
    State(state): State<Arc<ConsoleState>>,
    CurrentUser(user): CurrentUser,
    headers: HeaderMap,
) -> Response {
    // Re-derive the CSRF token from the current session so the JS client can
    // recover it after a page refresh without a re-login.
    let csrf = super::auth::extract_session_id(&headers, &state.config.console.session_secret)
        .and_then(|sid| {
            state
                .db
                .with_conn(|conn| db::session_by_id(conn, &sid))
                .ok()
                .flatten()
                .and_then(|s| s.csrf_token)
        })
        .map(|raw| encode_csrf_token(&raw));

    let user_json = serde_json::to_value(UserResponse::from(user)).unwrap_or_else(|_| json!({}));
    let mut body = match user_json {
        serde_json::Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    if let Some(c) = csrf {
        body.insert("csrf_token".into(), serde_json::Value::String(c));
    }
    Json(serde_json::Value::Object(body)).into_response()
}

// ── API: Tokens (self-service) ──────────────────────────────────────────────

pub async fn api_list_tokens(
    State(state): State<Arc<ConsoleState>>,
    CurrentUser(user): CurrentUser,
) -> Response {
    let result = state
        .db
        .with_conn(|conn| Ok(token_store::list_for_user(conn, user.id)?));
    match result {
        Ok(rows) => {
            let items: Vec<_> = rows
                .into_iter()
                .map(|r| {
                    json!({
                        "id": r.id,
                        "prefix": r.prefix,
                        "label": r.label,
                        "created_at": r.created_at,
                        "expires_at": r.expires_at,
                        "revoked_at": r.revoked_at,
                        "last_used_at": r.last_used_at,
                    })
                })
                .collect();
            Json(json!({ "data": items })).into_response()
        }
        Err(e) => {
            tracing::warn!("list_tokens: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "failed to list tokens"})),
            )
                .into_response()
        }
    }
}

pub async fn api_create_token(
    State(state): State<Arc<ConsoleState>>,
    MutatingUser { user, session_id }: MutatingUser,
    Json(body): Json<CreateTokenRequest>,
) -> Response {
    let label_trimmed = body.label.trim();
    if label_trimmed.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "label is required"})),
        )
            .into_response();
    }

    if let Some(ref exp) = body.expires_at {
        if chrono::DateTime::parse_from_rfc3339(exp).is_err() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "expires_at must be RFC3339"})),
            )
                .into_response();
        }
    }

    let env_marker = state.config.auth.env_marker;

    const MAX_ATTEMPTS: usize = 3;
    let mut last_err = None;
    let mut minted = None;

    for attempt in 1..=MAX_ATTEMPTS {
        let token = Token::generate(env_marker);
        let insert_result = state.db.with_conn(|conn| {
            Ok(token_store::insert(
                conn,
                &token.prefix,
                &token.hash,
                user.id,
                Some(label_trimmed),
                body.expires_at.as_deref(),
            )?)
        });
        match insert_result {
            Ok(id) => {
                minted = Some((id, token));
                break;
            }
            Err(e) => {
                let msg = format!("{e}");
                let is_collision = msg.contains("UNIQUE constraint failed")
                    && msg.contains("client_tokens.prefix");
                if is_collision && attempt < MAX_ATTEMPTS {
                    tracing::warn!("create_token: prefix collision, retrying");
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
                "create_token: failed after {} attempts: {:?}",
                MAX_ATTEMPTS,
                last_err
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "failed to persist token"})),
            )
                .into_response();
        }
    };

    if let Some(ref log) = state.audit_log {
        log.record_mutation(
            &MutationRecord::new(
                &user.username,
                user.id,
                "token_create",
                format!("{} created token \"{}\"", user.username, label_trimmed),
            )
            .with_target(user.username.clone())
            .with_after(json!({
                "id": id,
                "prefix": token.prefix,
                "label": label_trimmed,
                "expires_at": body.expires_at,
            })),
        );
    }

    let next_csrf = rotate_csrf(&state, &session_id);
    let headers = csrf_next_headers(next_csrf.as_deref());
    (
        StatusCode::CREATED,
        headers,
        Json(json!({
            "id": id,
            "prefix": token.prefix,
            "token": token.wire,
            "warning": "Store this token now — it is not recoverable from any later API call.",
            "label": label_trimmed,
            "expires_at": body.expires_at,
        })),
    )
        .into_response()
}

pub async fn api_revoke_token(
    State(state): State<Arc<ConsoleState>>,
    MutatingUser { user, session_id }: MutatingUser,
    Path(id): Path<i64>,
) -> Response {
    let owned_row = state.db.with_conn(|conn| {
        let rows = token_store::list_for_user(conn, user.id)?;
        Ok(rows.into_iter().find(|r| r.id == id))
    });

    let row = match owned_row {
        Ok(Some(r)) => r,
        Ok(None) => {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "token does not belong to you"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::warn!("revoke_token: ownership check failed: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal error"})),
            )
                .into_response();
        }
    };

    match state
        .db
        .with_conn(|conn| Ok(token_store::revoke(conn, id)?))
    {
        Ok(affected) => {
            if let Some(ref log) = state.audit_log {
                log.record_mutation(
                    &MutationRecord::new(
                        &user.username,
                        user.id,
                        "token_revoke",
                        format!(
                            "{} revoked token \"{}\"",
                            user.username,
                            row.label.as_deref().unwrap_or(&row.prefix)
                        ),
                    )
                    .with_target(user.username.clone())
                    .with_before(json!({
                        "id": row.id,
                        "prefix": row.prefix,
                        "label": row.label,
                        "revoked_at": row.revoked_at,
                    }))
                    .with_after(json!({
                        "id": row.id,
                        "prefix": row.prefix,
                        "label": row.label,
                        "revoked": true,
                    })),
                );
            }
            // Cross-process invalidation. The proxy keeps an in-memory
            // verification cache (default 60s TTL) keyed by token prefix.
            // The Web Console runs as a separate process, so calling
            // `invalidate_cached` here would only touch *our* (empty)
            // cache. Send the dedicated `INVALIDATE_TOKENS` command over
            // the configured [reload].socket (or, if only [reload].pid_file
            // is configured, fall back to SIGHUP — heavier, same end).
            //
            // Surface the trigger outcome in the response body so an
            // operator who configured `[reload]` and is watching for
            // revoke-then-200 regressions can spot a misconfigured
            // socket / pid_file at exactly the moment it bites them.
            let reload = super::reload::trigger_invalidate_tokens(&state.config.reload).await;
            let next_csrf = rotate_csrf(&state, &session_id);
            let headers = csrf_next_headers(next_csrf.as_deref());
            (
                StatusCode::OK,
                headers,
                Json(json!({
                    "id": id,
                    "status": "revoked",
                    "affected": affected,
                    "reload": {
                        "triggered": reload.triggered,
                        "method": reload.method,
                        "error": reload.error,
                    },
                })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::warn!("revoke_token: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "failed to revoke token"})),
            )
                .into_response()
        }
    }
}

// ── API: Audit log (read-only) ──────────────────────────────────────────────

pub async fn api_audit(
    State(state): State<Arc<ConsoleState>>,
    CurrentUser(user): CurrentUser,
    Query(q): Query<AuditQuery>,
) -> Response {
    let path = &state.config.audit.path;
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                return Json(json!({ "data": [] })).into_response();
            }
            tracing::warn!("audit: read failed: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "failed to read audit log"})),
            )
                .into_response();
        }
    };

    let limit = q.limit.unwrap_or(100);
    let mut entries = Vec::new();

    for line in content.lines().rev().take(limit * 2) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        if let Some(ref v) = q.verdict {
            if obj.get("verdict").and_then(|x| x.as_str()) != Some(v) {
                continue;
            }
        }

        if user.role != "admin" {
            let entry_user_id = obj.get("user_id").and_then(|x| x.as_str());
            let my_id = user.id.to_string();
            if entry_user_id != Some(&my_id) {
                continue;
            }
        }

        entries.push(obj);
        if entries.len() >= limit {
            break;
        }
    }

    Json(json!({ "data": entries })).into_response()
}

// ── API: Budget (read-only) ─────────────────────────────────────────────────

pub async fn api_budget(
    State(state): State<Arc<ConsoleState>>,
    CurrentUser(user): CurrentUser,
) -> Response {
    let db_path = &state.config.budget.db_path;

    let conn = match rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("budget: open ro failed: {}", e);
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "budget database unavailable"})),
            )
                .into_response();
        }
    };

    let mut items = Vec::new();

    if user.role == "admin" {
        let mut stmt = match conn.prepare(
            "SELECT u.api_key, COALESCE(u.total_tokens, 0) as usage, l.token_limit
             FROM api_key_usage u
             LEFT JOIN api_key_limits l ON u.api_key = l.api_key
             ORDER BY usage DESC",
        ) {
            Ok(s) => s,
            Err(_) => {
                return Json(json!({ "data": [] })).into_response();
            }
        };
        let rows = stmt.query_map([], |row| {
            Ok(json!({
                "api_key": row.get::<_, String>(0)?,
                "usage": row.get::<_, u64>(1)?,
                "limit": row.get::<_, Option<u64>>(2)?,
            }))
        });
        if let Ok(rows) = rows {
            items = rows.filter_map(|r| r.ok()).collect();
        }
    } else {
        let prefixes: Vec<String> = state
            .db
            .with_conn(|conn| {
                let rows = token_store::list_for_user(conn, user.id)?;
                Ok(rows.into_iter().map(|r| r.prefix).collect())
            })
            .unwrap_or_default();

        for prefix in prefixes {
            let usage: u64 = conn
                .query_row(
                    "SELECT COALESCE(total_tokens, 0) FROM api_key_usage WHERE api_key = ?",
                    params![prefix],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            let limit: Option<u64> = conn
                .query_row(
                    "SELECT token_limit FROM api_key_limits WHERE api_key = ?",
                    params![prefix],
                    |row| row.get(0),
                )
                .optional()
                .unwrap_or(None);
            items.push(json!({
                "api_key": prefix,
                "usage": usage,
                "limit": limit,
            }));
        }
    }

    Json(json!({ "data": items })).into_response()
}

// ── API: Budget admin (limit set + usage reset) ────────────────────────────

#[derive(Deserialize)]
pub struct SetBudgetLimitRequest {
    pub api_key: String,
    /// Per-API-key cap in tokens. Pass `null` (or omit) to clear an
    /// existing limit. The SQLite store maps "no row" to "unlimited",
    /// so a missing limit is functionally unlimited.
    #[serde(default)]
    pub limit: Option<u64>,
}

/// `POST /api/budget/limit` — set or clear a per-key budget cap from
/// the Console. Mirrors `PUT /v1/admin/budget/:api_key` on the proxy,
/// but it lives inside the Console process so an operator who only
/// has the Console URL and a Console session does not also need a
/// proxy `ADMIN_API_KEY` to manage limits.
pub async fn api_set_budget_limit(
    State(state): State<Arc<ConsoleState>>,
    MutatingUser {
        user: admin,
        session_id,
    }: MutatingUser,
    Json(body): Json<SetBudgetLimitRequest>,
) -> Response {
    if let Err(e) = require_admin(&admin) {
        return *e;
    }

    let api_key = body.api_key.trim();
    if api_key.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "api_key is required"})),
        )
            .into_response();
    }

    let db_path = &state.config.budget.db_path;
    let conn = match rusqlite::Connection::open(db_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("budget: open rw failed: {}", e);
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "budget database unavailable"})),
            )
                .into_response();
        }
    };

    let result = match body.limit {
        Some(limit) => {
            // Upsert the limit, then make sure an api_key_usage row
            // exists too. The admin /api/budget reader JOINs from
            // usage → limits, so a freshly-set limit on a never-used
            // key would otherwise be invisible (no usage row → no
            // JOIN match → no row in the response) and the operator
            // would think the save didn't take.
            let r1 = conn.execute(
                "INSERT INTO api_key_limits (api_key, token_limit)
                 VALUES (?1, ?2)
                 ON CONFLICT(api_key) DO UPDATE SET token_limit = excluded.token_limit",
                params![api_key, limit],
            );
            if r1.is_ok() {
                let _ = conn.execute(
                    "INSERT INTO api_key_usage (api_key, total_tokens)
                     VALUES (?1, 0)
                     ON CONFLICT(api_key) DO NOTHING",
                    params![api_key],
                );
            }
            r1
        }
        None => conn.execute(
            "DELETE FROM api_key_limits WHERE api_key = ?1",
            params![api_key],
        ),
    };

    if let Err(e) = result {
        tracing::warn!("budget: write failed: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to write budget limit"})),
        )
            .into_response();
    }

    if let Some(ref log) = state.audit_log {
        log.record_mutation(
            &MutationRecord::new(
                &admin.username,
                admin.id,
                "budget_set_limit",
                format!(
                    "{} set budget limit for `{}` to {}",
                    admin.username,
                    api_key,
                    body.limit
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "unlimited".to_string())
                ),
            )
            .with_target(api_key.to_string())
            .with_after(json!({ "api_key": api_key, "limit": body.limit })),
        );
    }

    let next_csrf = rotate_csrf(&state, &session_id);
    let headers = csrf_next_headers(next_csrf.as_deref());
    (
        StatusCode::OK,
        headers,
        Json(json!({ "api_key": api_key, "limit": body.limit })),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct ResetBudgetUsageRequest {
    pub api_key: String,
}

/// `POST /api/budget/reset` — zero the usage counter for an API key.
/// Mirrors `DELETE /v1/admin/budget/:api_key/reset` on the proxy.
pub async fn api_reset_budget_usage(
    State(state): State<Arc<ConsoleState>>,
    MutatingUser {
        user: admin,
        session_id,
    }: MutatingUser,
    Json(body): Json<ResetBudgetUsageRequest>,
) -> Response {
    if let Err(e) = require_admin(&admin) {
        return *e;
    }

    let api_key = body.api_key.trim();
    if api_key.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "api_key is required"})),
        )
            .into_response();
    }

    let db_path = &state.config.budget.db_path;
    let conn = match rusqlite::Connection::open(db_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("budget: open rw failed: {}", e);
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "budget database unavailable"})),
            )
                .into_response();
        }
    };

    if let Err(e) = conn.execute(
        "INSERT INTO api_key_usage (api_key, total_tokens) VALUES (?1, 0)
         ON CONFLICT(api_key) DO UPDATE SET total_tokens = 0",
        params![api_key],
    ) {
        tracing::warn!("budget: reset failed: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to reset budget usage"})),
        )
            .into_response();
    }

    if let Some(ref log) = state.audit_log {
        log.record_mutation(
            &MutationRecord::new(
                &admin.username,
                admin.id,
                "budget_reset_usage",
                format!("{} reset budget usage for `{}`", admin.username, api_key),
            )
            .with_target(api_key.to_string()),
        );
    }

    let next_csrf = rotate_csrf(&state, &session_id);
    let headers = csrf_next_headers(next_csrf.as_deref());
    (
        StatusCode::OK,
        headers,
        Json(json!({ "api_key": api_key, "reset": true })),
    )
        .into_response()
}

// ── API: Backends (admin only) — multi-backend routing ────────────────────
//
// These handlers read and rewrite the `[backends.*]` section of
// `nanoguard.toml` on disk. The proxy's live backend pool is
// restart-only (see docs/design/multi-backend-routing.md > State
// management — orphaning a `reqwest` connection pool mid-request is
// unsafe), so a "Save" here updates the on-disk config and fires a
// reload trigger, but the proxy's live pool only picks up
// new/removed entries on the next process restart. Routing rules
// (the `[routing]` section) ARE hot-reloadable; those land in a
// separate handler.
//
// The handlers use `toml_edit` to round-trip the TOML so unrelated
// sections, comments, and whitespace stay verbatim. Atomic rename
// + reload trigger come from the same helpers the file-edit
// machinery uses.

/// Backend create/update request body.
///
/// `api_key` is a deliberate three-state value to avoid the
/// "operator hit Save without retyping the key and we silently
/// cleared it" footgun:
///
/// - field omitted from JSON  → `None`           → keep stored key
/// - `"api_key": null`        → `Some(None)`     → clear stored key
/// - `"api_key": "sk-..."`    → `Some(Some(s))`  → replace with `s`
///
/// `serde(default, with = ...)` realizes the distinction via the
/// double-Option deserializer below.
#[derive(Deserialize)]
pub struct BackendUpsertRequest {
    pub provider: String,
    pub endpoint: String,
    #[serde(default, deserialize_with = "deserialize_some_option")]
    pub api_key: Option<Option<String>>,
    #[serde(default)]
    pub model: Option<String>,
}

fn deserialize_some_option<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // If the field is present, deserialize as Option<String> (null
    // becomes Some(None) via the outer Option wrapping). If the
    // field is absent, serde's `default` short-circuits to None,
    // which the handler reads as "keep current".
    Option::<String>::deserialize(deserializer).map(Some)
}

/// Re-parse nanoguard.toml from disk. The ConsoleState holds the
/// startup snapshot for stable reads, but Backend CRUD writes the
/// on-disk file and we need to see those writes in subsequent
/// reads without a process restart. Used by the Backends handlers
/// for both the list and the existence checks.
fn fresh_config() -> anyhow::Result<crate::config::Config> {
    crate::config::Config::from_env_or_default()
}

/// `GET /api/backends` — list configured backends. Admin-only;
/// listing backends reveals upstream provider URLs and is not
/// information a viewer-role user needs.
pub async fn api_list_backends(
    State(_state): State<Arc<ConsoleState>>,
    CurrentUser(user): CurrentUser,
) -> Response {
    if let Err(e) = require_admin(&user) {
        return *e;
    }

    let cfg = match fresh_config() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("re-parse failed: {e}")})),
            )
                .into_response();
        }
    };

    let (pool_view, _) = match cfg.pool() {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("config invalid: {e}")})),
            )
                .into_response();
        }
    };

    let data: Vec<serde_json::Value> = pool_view
        .backends
        .iter()
        .map(|(name, b)| {
            json!({
                "name": name,
                "provider": b.provider,
                "endpoint": b.endpoint,
                "model": b.model,
                "has_api_key": b.api_key.as_deref().map(|s| !s.is_empty()).unwrap_or(false),
                "is_default": name == &pool_view.default_backend,
            })
        })
        .collect();
    Json(json!({"data": data, "default": pool_view.default_backend})).into_response()
}

/// `POST /api/backends?name=<name>` — add a backend. Returns 409 if
/// `<name>` already exists.
pub async fn api_create_backend(
    State(state): State<Arc<ConsoleState>>,
    MutatingUser {
        user: admin,
        session_id,
    }: MutatingUser,
    Query(q): Query<BackendNameQuery>,
    Json(body): Json<BackendUpsertRequest>,
) -> Response {
    if let Err(e) = require_admin(&admin) {
        return *e;
    }

    let name = q.name.trim();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "name query parameter is required"})),
        )
            .into_response();
    }
    if let Err(msg) = validate_backend_name(name) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response();
    }
    if let Err(msg) = validate_backend_body(&body) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response();
    }

    let cfg = match fresh_config() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e:#}")})),
            )
                .into_response();
        }
    };
    if cfg.backends.contains_key(name) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": format!("backend `{name}` already exists; PUT to update")})),
        )
            .into_response();
    }

    if let Err(e) = upsert_backend_in_toml(name, &body) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e:#}")})),
        )
            .into_response();
    }

    record_backend_mutation(&state, &admin, "backend_create", name, Some(&body));
    let reload = super::reload::trigger_reload(&state.config.reload).await;
    let next_csrf = rotate_csrf(&state, &session_id);
    let headers = csrf_next_headers(next_csrf.as_deref());
    (
        StatusCode::CREATED,
        headers,
        Json(json!({
            "name": name,
            "restart_required": false,
            "reload": reload_outcome_json(&reload),
        })),
    )
        .into_response()
}

/// `PUT /api/backends/:name` — replace a backend's fields. 404 when
/// the backend does not exist.
pub async fn api_update_backend(
    State(state): State<Arc<ConsoleState>>,
    MutatingUser {
        user: admin,
        session_id,
    }: MutatingUser,
    Path(name): Path<String>,
    Json(body): Json<BackendUpsertRequest>,
) -> Response {
    if let Err(e) = require_admin(&admin) {
        return *e;
    }

    if let Err(msg) = validate_backend_name(&name) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response();
    }
    if let Err(msg) = validate_backend_body(&body) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response();
    }

    let cfg = match fresh_config() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e:#}")})),
            )
                .into_response();
        }
    };
    if !cfg.backends.contains_key(&name) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("backend `{name}` not found")})),
        )
            .into_response();
    }

    if let Err(e) = upsert_backend_in_toml(&name, &body) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e:#}")})),
        )
            .into_response();
    }

    record_backend_mutation(&state, &admin, "backend_update", &name, Some(&body));
    let reload = super::reload::trigger_reload(&state.config.reload).await;
    let next_csrf = rotate_csrf(&state, &session_id);
    let headers = csrf_next_headers(next_csrf.as_deref());
    (
        StatusCode::OK,
        headers,
        Json(json!({
            "name": name,
            "restart_required": false,
            "reload": reload_outcome_json(&reload),
        })),
    )
        .into_response()
}

/// `DELETE /api/backends/:name` — drop a backend from the TOML.
/// Refuses to remove the routing default; the operator must point
/// `[routing].default` at a different label first.
pub async fn api_delete_backend(
    State(state): State<Arc<ConsoleState>>,
    MutatingUser {
        user: admin,
        session_id,
    }: MutatingUser,
    Path(name): Path<String>,
) -> Response {
    if let Err(e) = require_admin(&admin) {
        return *e;
    }
    let cfg = match fresh_config() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e:#}")})),
            )
                .into_response();
        }
    };
    if !cfg.backends.contains_key(&name) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("backend `{name}` not found")})),
        )
            .into_response();
    }
    if cfg.routing.default.as_deref() == Some(name.as_str()) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": format!(
                "cannot delete backend `{name}`: it is the routing default. \
                 Change [routing].default first."
            )})),
        )
            .into_response();
    }
    if cfg.routing.rules.iter().any(|r| r.backend == name) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": format!(
                "cannot delete backend `{name}`: at least one [routing] rule references it. \
                 Update the rules first."
            )})),
        )
            .into_response();
    }

    if let Err(e) = delete_backend_in_toml(&name) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e:#}")})),
        )
            .into_response();
    }

    record_backend_mutation(&state, &admin, "backend_delete", &name, None);
    let reload = super::reload::trigger_reload(&state.config.reload).await;
    let next_csrf = rotate_csrf(&state, &session_id);
    let headers = csrf_next_headers(next_csrf.as_deref());
    (
        StatusCode::OK,
        headers,
        Json(json!({
            "name": name,
            "restart_required": false,
            "reload": reload_outcome_json(&reload),
        })),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct BackendNameQuery {
    pub name: String,
}

// ── API: Routing (admin only) ─────────────────────────────────────────────
//
// `[routing]` is the hot-reloadable companion to the restart-only
// `[backends.*]` map: the operator chooses which configured backend
// serves a given request, by model pattern (`first-match-wins`) with
// a default fallback. The Backends tab already covers the pool side;
// this block adds a `PUT /api/routing` write surface so the routing
// table can be edited from the UI without raw TOML editing.

#[derive(Deserialize)]
pub struct RoutingUpdateRequest {
    /// Required: the backend label to use when no rule matches.
    pub default: String,
    /// Rules scanned in declared order, first match wins. Empty list
    /// is fine — the proxy just uses `default` for every request.
    #[serde(default)]
    pub rules: Vec<RoutingRuleSpec>,
}

#[derive(Deserialize, Clone)]
pub struct RoutingRuleSpec {
    pub model: String,
    pub backend: String,
    /// Ordered list of additional backend labels to try if `backend`
    /// fails on /v1/chat/completions or /v1/messages (network error
    /// or upstream status >= 500). Empty list is the default and
    /// means no failover.
    #[serde(default)]
    pub fallback: Vec<String>,
}

/// `PUT /api/routing` — replace the entire `[routing]` section. We
/// take the whole table rather than diff-y endpoints because the
/// order of `rules` is semantically meaningful (first-match-wins),
/// and small operations like "swap rule 2 and 3" are clearer as
/// "send me the new full list" than as a JSON Patch.
pub async fn api_update_routing(
    State(state): State<Arc<ConsoleState>>,
    MutatingUser {
        user: admin,
        session_id,
    }: MutatingUser,
    Json(mut body): Json<RoutingUpdateRequest>,
) -> Response {
    if let Err(e) = require_admin(&admin) {
        return *e;
    }

    // Normalize whitespace once at the boundary so validation, write,
    // audit, and the response echo all observe the same string. Leaving
    // whitespace in for the writer would let a payload like `" alpha "`
    // pass validation (which trims) but persist verbatim, which then
    // fails to match any backend key on the next reload.
    body.default = body.default.trim().to_string();
    for r in body.rules.iter_mut() {
        r.model = r.model.trim().to_string();
        r.backend = r.backend.trim().to_string();
        r.fallback = r
            .fallback
            .iter()
            .map(|f| f.trim().to_string())
            .filter(|f| !f.is_empty())
            .collect();
    }

    let cfg = match fresh_config() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e:#}")})),
            )
                .into_response();
        }
    };
    if let Err(msg) = validate_routing_body(&body, &cfg) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response();
    }

    let prev_default = cfg.routing.default.clone();
    let prev_rule_count = cfg.routing.rules.len();

    if let Err(e) = write_routing_to_toml(&body) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e:#}")})),
        )
            .into_response();
    }

    record_routing_mutation(
        &state,
        &admin,
        &body,
        prev_default.as_deref(),
        prev_rule_count,
    );
    let reload = super::reload::trigger_reload(&state.config.reload).await;
    let next_csrf = rotate_csrf(&state, &session_id);
    let headers = csrf_next_headers(next_csrf.as_deref());
    (
        StatusCode::OK,
        headers,
        Json(json!({
            "default": body.default,
            "rules": body.rules.iter().map(|r| json!({
                "model": r.model,
                "backend": r.backend,
                "fallback": r.fallback,
            })).collect::<Vec<_>>(),
            // `[routing]` is hot-reloadable (see CLAUDE.md and
            // docs/design/multi-backend-routing.md), so the change
            // takes effect on the next reload, not on process
            // restart. Flag it for the SPA so the operator sees the
            // right toast.
            "restart_required": false,
            "reload": reload_outcome_json(&reload),
        })),
    )
        .into_response()
}

fn validate_routing_body(
    body: &RoutingUpdateRequest,
    cfg: &crate::config::Config,
) -> std::result::Result<(), String> {
    let default = body.default.trim();
    if default.is_empty() {
        return Err("routing.default cannot be empty".into());
    }
    if !cfg.backends.contains_key(default) {
        return Err(format!(
            "routing.default = `{default}` is not a configured backend"
        ));
    }
    let mut seen_models = std::collections::HashSet::new();
    for (i, rule) in body.rules.iter().enumerate() {
        let model = rule.model.trim();
        let backend = rule.backend.trim();
        if model.is_empty() {
            return Err(format!("rule[{i}].model cannot be empty"));
        }
        if backend.is_empty() {
            return Err(format!("rule[{i}].backend cannot be empty"));
        }
        if !cfg.backends.contains_key(backend) {
            return Err(format!(
                "rule[{i}].backend = `{backend}` is not a configured backend"
            ));
        }
        // Fallbacks must (a) all reference real backends, (b) not name
        // the primary (no self-loop), and (c) not repeat a label within
        // the same rule. `cfg.pool()` re-validates (a) and (b) at
        // reload time, but catching the operator error here means the
        // PUT returns a friendly 400 instead of a reload_failed audit
        // entry the operator has to dig out of a log file.
        let mut seen_fb = std::collections::HashSet::new();
        for (fi, fb) in rule.fallback.iter().enumerate() {
            let fb = fb.trim();
            if fb.is_empty() {
                return Err(format!("rule[{i}].fallback[{fi}] cannot be empty"));
            }
            if !cfg.backends.contains_key(fb) {
                return Err(format!(
                    "rule[{i}].fallback[{fi}] = `{fb}` is not a configured backend"
                ));
            }
            if fb == backend {
                return Err(format!(
                    "rule[{i}].fallback[{fi}] = `{fb}` is the same as the rule's primary backend"
                ));
            }
            if !seen_fb.insert(fb.to_string()) {
                return Err(format!(
                    "rule[{i}].fallback[{fi}] = `{fb}` is repeated in the same rule"
                ));
            }
        }
        if !seen_models.insert(model.to_string()) {
            // Duplicate model patterns are almost certainly a typo —
            // first-match-wins means the later entry can never fire.
            // Fail loud so the operator can fix the source pattern.
            return Err(format!(
                "rule[{i}].model = `{model}` is duplicated; later occurrence would never fire"
            ));
        }
    }
    Ok(())
}

/// Round-trip `nanoguard.toml`: replace the `[routing]` section's
/// `default` and `rules` while leaving every other section, comment,
/// and key ordering verbatim. Atomic-write + backup like every other
/// console edit.
fn write_routing_to_toml(body: &RoutingUpdateRequest) -> anyhow::Result<()> {
    use std::fs;
    use toml_edit::{value, Array, InlineTable, Item, Table};
    let path_owned =
        std::env::var("NANOGUARD_CONFIG").unwrap_or_else(|_| "nanoguard.toml".to_string());
    let path = path_owned.as_str();
    let original = fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading {path}: {e}"))?;
    let mut doc = original
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| anyhow::anyhow!("parsing {path}: {e}"))?;

    let routing_section = doc
        .entry("routing")
        .or_insert_with(|| Item::Table(Table::new()));
    let routing_tbl = routing_section
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[routing] in {path} is not a table"))?;

    routing_tbl.insert("default", value(body.default.clone()));

    let mut rules = Array::new();
    for r in &body.rules {
        let mut t = InlineTable::new();
        t.insert("model", r.model.clone().into());
        t.insert("backend", r.backend.clone().into());
        if !r.fallback.is_empty() {
            // Only emit `fallback = [...]` when there is at least one
            // entry, so configs that don't use failover stay
            // byte-for-byte identical to pre-fallback ones on disk.
            let mut arr = Array::new();
            for fb in &r.fallback {
                arr.push(fb.clone());
            }
            t.insert("fallback", arr.into());
        }
        rules.push(t);
    }
    routing_tbl.insert("rules", Item::Value(rules.into()));

    let serialized = doc.to_string();
    super::edit::atomic_write(path, &serialized, |_| super::edit::ValidationResult {
        valid: true,
        error: None,
    })?;
    Ok(())
}

fn record_routing_mutation(
    state: &Arc<ConsoleState>,
    admin: &crate::console::db::User,
    body: &RoutingUpdateRequest,
    prev_default: Option<&str>,
    prev_rule_count: usize,
) {
    if let Some(ref log) = state.audit_log {
        let summary = format!(
            "{} updated [routing]: default `{}` → `{}`, rules {} → {}",
            admin.username,
            prev_default.unwrap_or("(unset)"),
            body.default,
            prev_rule_count,
            body.rules.len(),
        );
        let rec = MutationRecord::new(&admin.username, admin.id, "routing_update", summary)
            .with_target("routing".to_string())
            .with_after(json!({
                "default": body.default,
                "rules": body.rules.iter().map(|r| json!({
                    "model": r.model,
                    "backend": r.backend,
                    "fallback": r.fallback,
                })).collect::<Vec<_>>(),
            }));
        log.record_mutation(&rec);
    }
}

fn validate_backend_name(name: &str) -> std::result::Result<(), String> {
    if name.is_empty() {
        return Err("backend name cannot be empty".into());
    }
    if name.len() > 64 {
        return Err("backend name too long (max 64 chars)".into());
    }
    // Keep names TOML-bare-key-safe so the round-trip stays sane: a
    // name that needs quoting would force us to choose a quoting
    // style and roundtrip it through toml_edit's escape logic; not
    // worth the surface for an operator label.
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("backend name must match [A-Za-z0-9_-]+".into());
    }
    Ok(())
}

fn validate_backend_body(body: &BackendUpsertRequest) -> std::result::Result<(), String> {
    match body.provider.as_str() {
        "openai" | "anthropic" | "ollama" => {}
        other => {
            return Err(format!(
                "unknown provider `{other}` (openai|anthropic|ollama)"
            ))
        }
    }
    let endpoint = body.endpoint.trim();
    if endpoint.is_empty() {
        return Err("endpoint cannot be empty".into());
    }
    if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
        return Err("endpoint must start with http:// or https://".into());
    }
    Ok(())
}

/// Round-trip `nanoguard.toml`: parse via `toml_edit::DocumentMut`
/// so comments/whitespace stay verbatim, insert or replace the
/// `[backends.<name>]` section, atomically rename. Backed up to
/// `.nanoguard-backups/` like every other config edit.
fn upsert_backend_in_toml(name: &str, body: &BackendUpsertRequest) -> anyhow::Result<()> {
    use std::fs;
    // Honor NANOGUARD_CONFIG when set so e2e (and operators with a
    // non-default config location) actually edit the file the proxy
    // reads, not a hard-coded "nanoguard.toml" in the CWD.
    let path_owned =
        std::env::var("NANOGUARD_CONFIG").unwrap_or_else(|_| "nanoguard.toml".to_string());
    let path = path_owned.as_str();
    let original = fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading {path}: {e}"))?;
    let mut doc = original
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| anyhow::anyhow!("parsing {path}: {e}"))?;

    let backends_section = doc
        .entry("backends")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
    let backends_tbl = backends_section
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[backends] in {path} is not a table"))?;
    // Make [backends] itself implicit so it does not render as a
    // standalone empty header — only [backends.NAME] subtables show.
    backends_tbl.set_implicit(true);

    // Preserve the existing entry if there is one — so the api_key
    // 3-state semantics work: "keep current" leaves the stored value
    // exactly as it was on disk. A full insert would silently drop
    // any field not explicitly sent in the request.
    let existing = backends_tbl
        .get(name)
        .and_then(|i| i.as_table())
        .cloned()
        .unwrap_or_default();
    let mut entry = existing;

    entry.insert("provider", toml_edit::value(body.provider.clone()));
    entry.insert("endpoint", toml_edit::value(body.endpoint.clone()));
    match &body.api_key {
        // Omitted: keep stored key. No mutation.
        None => {}
        // Explicit null: clear stored key.
        Some(None) => {
            entry.remove("api_key");
        }
        // String: replace.
        Some(Some(k)) => {
            entry.insert("api_key", toml_edit::value(k.clone()));
        }
    }
    // `model` mirrors `api_key`'s "field omitted = keep current"
    // rule. Since `entry` is the cloned existing table, doing
    // nothing here preserves the prior model field. If serde
    // someday gives `model` the same 3-state shape, switch this
    // branch to follow.
    if let Some(ref m) = body.model {
        entry.insert("model", toml_edit::value(m.clone()));
    }
    backends_tbl.insert(name, toml_edit::Item::Table(entry));

    let serialized = doc.to_string();
    super::edit::atomic_write(path, &serialized, |_| super::edit::ValidationResult {
        valid: true,
        error: None,
    })?;
    Ok(())
}

fn delete_backend_in_toml(name: &str) -> anyhow::Result<()> {
    use std::fs;
    let path_owned =
        std::env::var("NANOGUARD_CONFIG").unwrap_or_else(|_| "nanoguard.toml".to_string());
    let path = path_owned.as_str();
    let original = fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading {path}: {e}"))?;
    let mut doc = original
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| anyhow::anyhow!("parsing {path}: {e}"))?;

    if let Some(backends) = doc.get_mut("backends").and_then(|i| i.as_table_mut()) {
        backends.remove(name);
    }

    let serialized = doc.to_string();
    super::edit::atomic_write(path, &serialized, |_| super::edit::ValidationResult {
        valid: true,
        error: None,
    })?;
    Ok(())
}

fn record_backend_mutation(
    state: &Arc<ConsoleState>,
    admin: &crate::console::db::User,
    action: &str,
    name: &str,
    body: Option<&BackendUpsertRequest>,
) {
    if let Some(ref log) = state.audit_log {
        let mut rec = MutationRecord::new(
            &admin.username,
            admin.id,
            action,
            format!(
                "{} {} backend `{}`",
                admin.username,
                action.trim_start_matches("backend_"),
                name
            ),
        )
        .with_target(name.to_string());
        if let Some(b) = body {
            // The audit shape for api_key reflects the request
            // intent, NOT the stored result — because the stored
            // value depends on the pre-mutation state (which the
            // audit writer doesn't see). null/false = caller asked
            // to clear; string/true = caller sent a new value;
            // omitted = caller asked to keep current. Operators
            // reading the audit log get the action, not just the
            // resulting state.
            let api_key_intent = match &b.api_key {
                None => json!("keep"),
                Some(None) => json!("clear"),
                Some(Some(_)) => json!("replace"),
            };
            rec = rec.with_after(json!({
                "name": name,
                "provider": b.provider,
                "endpoint": b.endpoint,
                "api_key_intent": api_key_intent,
                "model": b.model,
            }));
        }
        log.record_mutation(&rec);
    }
}

fn reload_outcome_json(reload: &super::reload::ReloadOutcome) -> serde_json::Value {
    json!({
        "triggered": reload.triggered,
        "method": reload.method,
        "error": reload.error,
    })
}

// ── API: Playground (admin only) — query proxy vs raw backend ─────────────
//
// Two-pane debugging surface: an operator pastes a chat-completions
// request, picks "through proxy" or "direct to backend", and gets
// the upstream response back in JSON so they can answer the
// recurring question "is this nanoguard blocking the request, or is
// the backend returning garbage?" The two endpoints share a
// `PlaygroundResponse` shape (status / latency / body / where), so
// the SPA can render the result of either call into the same panel.
//
// Why this lives in the console, not in the proxy itself:
//   - The proxy never exposes a mutation HTTP surface; the
//     playground reads operator config (api_key for the chosen
//     backend) and is admin-authenticated.
//   - The proxy-direction call goes back through nanoguard's own
//     /v1/chat/completions, which means the full guardrail
//     pipeline runs. That is the whole point — we want to see
//     what the guardrails do.
//   - The backend-direction call bypasses the proxy by design,
//     using the backend's `endpoint` + `api_key` from `[backends.*]`
//     verbatim. No guardrails, raw upstream behavior.

#[derive(Deserialize)]
pub struct PlaygroundProxyRequest {
    /// The full OpenAI-shape chat-completions body. We forward it
    /// verbatim; the operator owns its content (model, messages,
    /// max_tokens, etc).
    pub body: JsonValue,
    /// Optional Bearer token to attach when [auth].enabled. When
    /// omitted, no Authorization header goes on the wire — useful
    /// for the operator to test "what happens without a token".
    #[serde(default)]
    pub bearer: Option<String>,
}

#[derive(Deserialize)]
pub struct PlaygroundBackendRequest {
    /// Backend label from [backends.*]. The handler reads the
    /// `endpoint` + `api_key` from the live config (NOT from the
    /// caller) so an admin cannot exfiltrate a backend's api_key
    /// or aim the proxy at an arbitrary URL through this endpoint.
    pub backend: String,
    /// Full chat-completions body, forwarded as-is to
    /// `<endpoint>/v1/chat/completions`.
    pub body: JsonValue,
}

#[derive(Serialize)]
struct PlaygroundResponse {
    /// "proxy" | "backend:<label>" — surfaces in the SPA so the
    /// caller knows which pane to render the result into. Renamed
    /// from the reserved `where` so the field stays idiomatic JSON.
    #[serde(rename = "where")]
    where_: String,
    /// HTTP status code from the upstream call. 0 when the request
    /// never completed (connect error, timeout); see `error`.
    status: u16,
    latency_ms: u128,
    /// Parsed JSON when the upstream returned a JSON body, raw
    /// string otherwise. Most LLM backends respond JSON; non-JSON
    /// almost always means "auth required" / HTML error page.
    body: JsonValue,
    /// `Some(_)` when the call itself failed (DNS, connect, timeout).
    /// Status 0 always pairs with `Some(_)`. Status >= 100 always
    /// pairs with `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// `POST /api/playground/proxy` — send a chat-completions request
/// through the local proxy (= full guardrail pipeline). Admin-only.
pub async fn api_playground_proxy(
    State(state): State<Arc<ConsoleState>>,
    MutatingUser {
        user: admin,
        session_id,
    }: MutatingUser,
    Json(req): Json<PlaygroundProxyRequest>,
) -> Response {
    if let Err(e) = require_admin(&admin) {
        return *e;
    }
    let listen = &state.config.nanoguard.listen;
    let host_port = normalize_listen(listen);
    let url = format!("http://{host_port}/v1/chat/completions");

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return playground_error_response(&state, &admin, "proxy", &session_id, e.to_string());
        }
    };

    let t0 = std::time::Instant::now();
    let mut builder = client.post(&url).json(&req.body);
    if let Some(ref token) = req.bearer {
        builder = builder.bearer_auth(token);
    }
    let result = builder.send().await;
    let elapsed = t0.elapsed().as_millis();

    let resp = match result {
        Ok(r) => r,
        Err(e) => {
            return playground_error_response_with_latency(
                &state,
                &admin,
                "proxy",
                &session_id,
                elapsed,
                e.to_string(),
            );
        }
    };
    let status = resp.status().as_u16();
    let body = read_body_as_json(resp).await;

    let next_csrf = rotate_csrf(&state, &session_id);
    let headers = csrf_next_headers(next_csrf.as_deref());
    record_playground_mutation(&state, &admin, "playground_proxy", "proxy");
    let payload = PlaygroundResponse {
        where_: "proxy".to_string(),
        status,
        latency_ms: elapsed,
        body,
        error: None,
    };
    (StatusCode::OK, headers, Json(payload)).into_response()
}

/// `POST /api/playground/backend` — send the same request directly
/// to a specific backend, bypassing the proxy and all guardrails.
/// The operator picks the backend label; api_key is pulled from
/// the live config (not the caller).
pub async fn api_playground_backend(
    State(state): State<Arc<ConsoleState>>,
    MutatingUser {
        user: admin,
        session_id,
    }: MutatingUser,
    Json(req): Json<PlaygroundBackendRequest>,
) -> Response {
    if let Err(e) = require_admin(&admin) {
        return *e;
    }

    let cfg = match fresh_config() {
        Ok(c) => c,
        Err(e) => {
            // Surfacing the config-reparse failure still has to rotate
            // CSRF and audit — otherwise the operator's next mutation
            // gets a stale-token 403 and we'd have no record of the
            // failed playground call.
            return playground_error_typed(
                &state,
                &admin,
                &session_id,
                "backend",
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("re-parse failed: {e}"),
            );
        }
    };
    let backend = match cfg.backends.get(&req.backend) {
        Some(b) => b,
        None => {
            // Maybe the operator is on the legacy single-`[backend]`
            // schema; surface that as "default" so it appears in
            // the dropdown like everything else.
            if req.backend == "default" {
                if let Some(b) = cfg.backend.as_ref() {
                    return playground_send_backend(&state, &admin, &session_id, b, &req.body)
                        .await;
                }
            }
            return playground_error_typed(
                &state,
                &admin,
                &session_id,
                &format!("backend:{}", req.backend),
                StatusCode::NOT_FOUND,
                format!("backend `{}` not configured", req.backend),
            );
        }
    };
    playground_send_backend(&state, &admin, &session_id, backend, &req.body).await
}

/// Like `playground_error_response_with_latency`, but for cases where
/// the playground call never reaches the upstream (config parse, no
/// such backend). Still rotates CSRF, still writes a `playground_error`
/// audit row, but returns a real HTTP 4xx/5xx envelope instead of the
/// 200-with-`status: 0` "transport error" shape we use for connect
/// failures.
fn playground_error_typed(
    state: &Arc<ConsoleState>,
    admin: &crate::console::db::User,
    session_id: &[u8],
    where_: &str,
    code: StatusCode,
    err: String,
) -> Response {
    let next_csrf = rotate_csrf(state, session_id);
    let headers = csrf_next_headers(next_csrf.as_deref());
    record_playground_mutation(state, admin, "playground_error", where_);
    (code, headers, Json(json!({"error": err}))).into_response()
}

async fn playground_send_backend(
    state: &Arc<ConsoleState>,
    admin: &crate::console::db::User,
    session_id: &[u8],
    backend: &crate::config::BackendConfig,
    body: &JsonValue,
) -> Response {
    let url = format!(
        "{}/v1/chat/completions",
        backend.endpoint.trim_end_matches('/')
    );
    let label = format!("backend:{}", backend.provider);

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return playground_error_response(state, admin, &label, session_id, e.to_string());
        }
    };

    let t0 = std::time::Instant::now();
    let mut builder = client.post(&url).json(body);
    if let Some(ref k) = backend.api_key {
        builder = builder.bearer_auth(k);
    }
    let result = builder.send().await;
    let elapsed = t0.elapsed().as_millis();

    let resp = match result {
        Ok(r) => r,
        Err(e) => {
            return playground_error_response_with_latency(
                state,
                admin,
                &label,
                session_id,
                elapsed,
                e.to_string(),
            );
        }
    };
    let status = resp.status().as_u16();
    let body_json = read_body_as_json(resp).await;

    let next_csrf = rotate_csrf(state, session_id);
    let headers = csrf_next_headers(next_csrf.as_deref());
    record_playground_mutation(state, admin, "playground_backend", &label);
    let payload = PlaygroundResponse {
        where_: label,
        status,
        latency_ms: elapsed,
        body: body_json,
        error: None,
    };
    (StatusCode::OK, headers, Json(payload)).into_response()
}

fn playground_error_response(
    state: &Arc<ConsoleState>,
    admin: &crate::console::db::User,
    where_: &str,
    session_id: &[u8],
    err: String,
) -> Response {
    playground_error_response_with_latency(state, admin, where_, session_id, 0, err)
}

fn playground_error_response_with_latency(
    state: &Arc<ConsoleState>,
    admin: &crate::console::db::User,
    where_: &str,
    session_id: &[u8],
    latency_ms: u128,
    err: String,
) -> Response {
    let next_csrf = rotate_csrf(state, session_id);
    let headers = csrf_next_headers(next_csrf.as_deref());
    record_playground_mutation(state, admin, "playground_error", where_);
    let payload = PlaygroundResponse {
        where_: where_.to_string(),
        status: 0,
        latency_ms,
        body: JsonValue::Null,
        error: Some(err),
    };
    (StatusCode::OK, headers, Json(payload)).into_response()
}

async fn read_body_as_json(resp: reqwest::Response) -> JsonValue {
    // Try JSON first (most successful LLM responses), fall back to
    // a string so HTML/text errors aren't lost. Either way, the SPA
    // gets a single typed slot to render.
    let text = resp.text().await.unwrap_or_default();
    serde_json::from_str(&text).unwrap_or(JsonValue::String(text))
}

fn record_playground_mutation(
    state: &Arc<ConsoleState>,
    admin: &crate::console::db::User,
    action: &str,
    where_: &str,
) {
    // Playground requests are HTTP mutations from the operator and
    // can carry sensitive content; audit them so an organisation
    // can see who used this surface. We log the action + the
    // target (proxy / backend:<provider>) but never the body — the
    // body is the operator's prompt and could include secrets they
    // pasted in to test redaction.
    if let Some(ref log) = state.audit_log {
        log.record_mutation(
            &MutationRecord::new(
                &admin.username,
                admin.id,
                action,
                format!("{} ran playground request via {}", admin.username, where_),
            )
            .with_target(where_.to_string()),
        );
    }
}

/// Normalize a listen address (e.g. "0.0.0.0:8080") to something a
/// loopback HTTP client can dial ("localhost:8080"). Same logic as
/// `api_overview`'s `proxy_url` resolution; duplicating it here so
/// playground stays self-contained.
fn normalize_listen(listen: &str) -> String {
    if let Some(port) = listen.strip_prefix("0.0.0.0:") {
        format!("localhost:{port}")
    } else if let Some(port) = listen.strip_prefix("[::]:") {
        format!("localhost:{port}")
    } else {
        listen.to_string()
    }
}

// ── API: Overview / getting-started summary ────────────────────────────────

/// `GET /api/overview` — a single read-only snapshot the SPA needs to
/// render the "you can start using nanoguard like this" panel without
/// piecing it together from three other endpoints.
///
/// Returns the proxy's listen URL, the auth mode (so we know whether to
/// tell the user "no token required" or to point them at the Tokens
/// tab), and a digest of which guard families are currently active.
/// Read-only on purpose — this is the front door, not a settings page.
pub async fn api_overview(
    State(state): State<Arc<ConsoleState>>,
    CurrentUser(user): CurrentUser,
) -> Response {
    let cfg = &state.config;

    // Reconstruct the proxy URL the operator's clients should target.
    // [nanoguard].listen can be "0.0.0.0:8080" or "127.0.0.1:8080";
    // the wildcard is correct on the wire but useless in a curl
    // example, so substitute localhost for clarity. Anything else
    // (a real interface address) is left as-is so a multi-host
    // deploy still gets a useful value.
    let listen = &cfg.nanoguard.listen;
    let display_host_port = if let Some(port) = listen.strip_prefix("0.0.0.0:") {
        format!("localhost:{port}")
    } else if let Some(port) = listen.strip_prefix("[::]:") {
        format!("localhost:{port}")
    } else {
        listen.clone()
    };
    let proxy_url = format!("http://{display_host_port}");

    let auth_enabled = cfg.auth.enabled;
    let env_marker = cfg.auth.env_marker;

    // Guard digest: enough for the operator to see at a glance "yes
    // PII redaction is on, schema validation is off." Each entry is
    // {name, enabled, summary}. The SPA renders them as a list with
    // a green/grey dot.
    //
    // Child guards are gated by their parent pipeline switch: if
    // `[input].enabled = false` the whole input pass is skipped, so
    // showing `Input PII redaction` as ON when it never runs would
    // be misleading. The summary still shows the configured child
    // value so an operator who toggles `[input].enabled` back on can
    // see what will activate.
    let input_on = cfg.input.enabled;
    let output_on = cfg.output.enabled;
    let mut guards: Vec<serde_json::Value> = Vec::new();
    let input_master_note = if input_on {
        ""
    } else {
        " (input pipeline disabled)"
    };
    let output_master_note = if output_on {
        ""
    } else {
        " (output pipeline disabled)"
    };
    guards.push(json!({
        "name": "Input pipeline (master)",
        "enabled": input_on,
        "summary": if input_on { "running" } else { "disabled — child guards do not run" },
    }));
    guards.push(json!({
        "name": "Input keyword block list",
        "enabled": input_on,
        "summary": format!(
            "{} inline_block / {} inline_alert / {} inline_flag keywords{}",
            cfg.input.keyword.inline_block.len(),
            cfg.input.keyword.inline_alert.len(),
            cfg.input.keyword.inline_flag.len(),
            input_master_note,
        ),
    }));
    guards.push(json!({
        "name": "Input PII redaction",
        "enabled": input_on && cfg.input.pii.enabled,
        "summary": format!("action = {:?}{}", cfg.input.pii.action, input_master_note),
    }));
    guards.push(json!({
        "name": "Spotlighting",
        "enabled": input_on && cfg.input.spotlight.enabled,
        "summary": format!("method = {}{}", cfg.input.spotlight.method, input_master_note),
    }));
    guards.push(json!({
        "name": "Output pipeline (master)",
        "enabled": output_on,
        "summary": if output_on { "running" } else { "disabled — child guards do not run" },
    }));
    guards.push(json!({
        "name": "Output PII redaction",
        "enabled": output_on && cfg.output.pii.enabled,
        "summary": format!("action = {:?}{}", cfg.output.pii.action, output_master_note),
    }));
    guards.push(json!({
        "name": "Output schema validation",
        "enabled": output_on && cfg.output.schema.enabled,
        "summary": format!(
            "{} rule(s); on_violation = {}{}",
            cfg.output.schema.rules.len(),
            cfg.output.schema.on_violation,
            output_master_note,
        ),
    }));
    let tool_allow_len = cfg.tools.allow.as_ref().map_or(0, |v| v.len());
    guards.push(json!({
        "name": "Tool gate",
        "enabled": cfg.tools.enabled,
        "summary": format!(
            "{} allow / {} deny rule(s)",
            tool_allow_len,
            cfg.tools.deny.len(),
        ),
    }));
    let policy_bundle_path = cfg.policies.bundle_path.as_deref().unwrap_or("");
    guards.push(json!({
        "name": "Policy bundle",
        "enabled": !policy_bundle_path.is_empty(),
        "summary": if policy_bundle_path.is_empty() {
            "no bundle configured".to_string()
        } else {
            format!("path = {}", policy_bundle_path)
        },
    }));

    // Backend digest. Multi-backend routing is shipped: resolve the
    // pool view (with legacy [backend] → "default" synthesis) and
    // return one entry per backend label plus the routing table.
    let (pool_view, _) = cfg.pool().unwrap_or_else(|_| {
        // Pool resolution failed (e.g. routing rule pointing at an
        // unknown backend). The proxy is unlikely to be up either,
        // but rather than 500 the overview endpoint we return an
        // empty pool so the SPA can still render the rest of the
        // dashboard. The error will already be in the startup log.
        (
            crate::config::BackendPool {
                backends: std::collections::BTreeMap::new(),
                rules: Vec::new(),
                default_backend: String::new(),
            },
            Vec::new(),
        )
    });

    let backends: Vec<serde_json::Value> = pool_view
        .backends
        .iter()
        .map(|(name, b)| {
            json!({
                "name": name,
                "provider": b.provider,
                "endpoint": b.endpoint,
                "model": b.model,
                "is_default": name == &pool_view.default_backend,
            })
        })
        .collect();
    let routing = json!({
        "default": pool_view.default_backend,
        "rules": pool_view.rules.iter().map(|r| json!({
            "model": r.model,
            "backend": r.backend,
            "fallback": r.fallback,
        })).collect::<Vec<_>>(),
    });
    // Legacy single-backend digest stays under `backend` for SPA
    // compatibility. Pick the routing default rather than
    // `backends.first()` — with multiple backends `.first()` is
    // BTreeMap-alphabetical, which lies to old SPA builds about
    // which upstream is actually serving unmatched requests. The
    // default is what those callers used to see when only one
    // backend was configured.
    let backend = backends
        .iter()
        .find(|b| {
            b.get("name")
                .and_then(|v| v.as_str())
                .is_some_and(|n| n == pool_view.default_backend)
        })
        .cloned()
        .or_else(|| backends.first().cloned())
        .unwrap_or_else(|| json!({}));

    // Count this user's live (non-revoked) tokens so the SPA can
    // say "you have N tokens" inline, without making the operator
    // click into the Tokens tab to find out.
    let token_count = state
        .db
        .with_conn(|conn| Ok(token_store::list_for_user(conn, user.id)?))
        .map(|rows| rows.iter().filter(|r| r.revoked_at.is_none()).count())
        .unwrap_or(0);

    Json(json!({
        "proxy_url": proxy_url,
        "listen": listen,
        "auth": {
            "enabled": auth_enabled,
            "env_marker": env_marker,
        },
        "backend": backend,
        "backends": backends,
        "routing": routing,
        "guards": guards,
        "user_token_count": token_count,
        "endpoints": [
            "/v1/chat/completions",
            "/v1/messages",
            "/v1/models",
            "/health",
        ],
    }))
    .into_response()
}

// ── API: Config files (read-only) ───────────────────────────────────────────

pub async fn api_config(State(_state): State<Arc<ConsoleState>>) -> Response {
    let mut files: HashMap<String, String> = HashMap::new();

    if let Ok(content) = std::fs::read_to_string("nanoguard.toml") {
        files.insert("nanoguard.toml".to_string(), content);
    }

    if let Ok(entries) = std::fs::read_dir("dicts") {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("txt") {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    let key = path.to_string_lossy().to_string();
                    files.insert(key, content);
                }
            }
        }
    }

    if let Ok(entries) = std::fs::read_dir("policies") {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("yaml") {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    let key = path.to_string_lossy().to_string();
                    files.insert(key, content);
                }
            }
        }
    }

    Json(json!({ "files": files })).into_response()
}

// ── API: Users (admin only) ─────────────────────────────────────────────────

pub async fn api_list_users(
    State(state): State<Arc<ConsoleState>>,
    CurrentUser(user): CurrentUser,
) -> Response {
    if let Err(e) = require_admin(&user) {
        return *e;
    }

    match state.db.with_conn(db::list_users) {
        Ok(users) => {
            let items: Vec<UserResponse> = users.into_iter().map(Into::into).collect();
            Json(json!({ "data": items })).into_response()
        }
        Err(e) => {
            tracing::warn!("list_users: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "failed to list users"})),
            )
                .into_response()
        }
    }
}

pub async fn api_create_user(
    State(state): State<Arc<ConsoleState>>,
    MutatingUser {
        user: admin,
        session_id,
    }: MutatingUser,
    Json(body): Json<CreateUserRequest>,
) -> Response {
    if let Err(e) = require_admin(&admin) {
        return *e;
    }

    let username = body.username.trim();
    if username.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "username is required"})),
        )
            .into_response();
    }
    if body.password.len() < 12 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "password must be at least 12 characters"})),
        )
            .into_response();
    }
    if super::auth::is_common_password(&body.password) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "password is too common"})),
        )
            .into_response();
    }

    let hash = match super::auth::hash_password(&body.password) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("create_user: hash failed: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "password hash failed"})),
            )
                .into_response();
        }
    };

    let role = body.role.as_deref().unwrap_or("user");
    match state
        .db
        .with_conn(|conn| db::insert_user(conn, username, None, None, role, Some(&hash)))
    {
        Ok(id) => {
            if let Some(ref log) = state.audit_log {
                log.record_mutation(
                    &MutationRecord::new(
                        &admin.username,
                        admin.id,
                        "user_create",
                        format!("{} created user {} ({})", admin.username, username, role),
                    )
                    .with_target(username.to_string())
                    .with_after(json!({
                        "id": id,
                        "username": username,
                        "role": role,
                    })),
                );
            }
            let next_csrf = rotate_csrf(&state, &session_id);
            let headers = csrf_next_headers(next_csrf.as_deref());
            (
                StatusCode::CREATED,
                headers,
                Json(json!({
                    "id": id,
                    "username": username,
                    "role": role,
                })),
            )
                .into_response()
        }
        Err(e) => {
            let msg = format!("{e}");
            if msg.contains("UNIQUE constraint failed") {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({"error": "username already exists"})),
                )
                    .into_response();
            }
            tracing::warn!("create_user: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "failed to create user"})),
            )
                .into_response()
        }
    }
}

pub async fn api_update_user(
    State(state): State<Arc<ConsoleState>>,
    MutatingUser {
        user: admin,
        session_id,
    }: MutatingUser,
    Path(id): Path<i64>,
    Json(body): Json<UpdateUserRequest>,
) -> Response {
    if let Err(e) = require_admin(&admin) {
        return *e;
    }

    // Snapshot the target user before the update so the audit record can
    // include a before/after diff for every changed field. A failed lookup
    // is non-fatal — the update will still run, but the audit entry will
    // omit the `before` block.
    let before_user = state
        .db
        .with_conn(|conn| db::user_by_id(conn, id))
        .ok()
        .flatten();

    match state.db.with_conn(|conn| {
        db::update_user(
            conn,
            id,
            db::UserUpdate {
                display_name: body.display_name.as_deref(),
                email: body.email.as_deref(),
                role: body.role.as_deref(),
                disabled: body.disabled,
                allowed_models: body.allowed_models.as_deref(),
                budget_limit: body.budget_limit,
            },
        )
    }) {
        Ok(n) => {
            // If role changed, invalidate the target user's sessions (session
            // rotation on privilege escalation).
            let role_changed = match (&body.role, &before_user) {
                (Some(new), Some(b)) => new != &b.role,
                _ => false,
            };
            if role_changed {
                if let Some(ref target) = before_user {
                    let _ = state
                        .db
                        .with_conn(|conn| db::delete_user_sessions(conn, target.id));
                }
            }

            if let Some(ref log) = state.audit_log {
                let target_name = before_user
                    .as_ref()
                    .map(|u| u.username.clone())
                    .unwrap_or_else(|| format!("user#{id}"));

                // Role changes are emitted as their own action so an
                // operator filtering by `action=user_role_change` can find
                // every privilege escalation/de-escalation without scanning
                // every user_update.
                if role_changed {
                    log.record_mutation(
                        &MutationRecord::new(
                            &admin.username,
                            admin.id,
                            "user_role_change",
                            format!(
                                "{} changed role for {} from {} to {}",
                                admin.username,
                                target_name,
                                before_user.as_ref().map(|u| u.role.as_str()).unwrap_or("?"),
                                body.role.as_deref().unwrap_or("?"),
                            ),
                        )
                        .with_target(target_name.clone())
                        .with_before(json!({
                            "role": before_user.as_ref().map(|u| u.role.clone()),
                        }))
                        .with_after(json!({
                            "role": body.role.clone(),
                        })),
                    );
                }

                // Build a before/after pair that only includes fields the
                // caller actually touched.
                let mut before = serde_json::Map::new();
                let mut after = serde_json::Map::new();
                if let Some(ref v) = body.display_name {
                    before.insert(
                        "display_name".into(),
                        json!(before_user.as_ref().and_then(|u| u.display_name.clone())),
                    );
                    after.insert("display_name".into(), json!(v));
                }
                if let Some(ref v) = body.email {
                    before.insert(
                        "email".into(),
                        json!(before_user.as_ref().and_then(|u| u.email.clone())),
                    );
                    after.insert("email".into(), json!(v));
                }
                if let Some(ref v) = body.role {
                    before.insert(
                        "role".into(),
                        json!(before_user.as_ref().map(|u| u.role.clone())),
                    );
                    after.insert("role".into(), json!(v));
                }
                if let Some(v) = body.disabled {
                    before.insert(
                        "disabled".into(),
                        json!(before_user.as_ref().map(|u| u.disabled)),
                    );
                    after.insert("disabled".into(), json!(v));
                }
                if let Some(ref v) = body.allowed_models {
                    before.insert(
                        "allowed_models".into(),
                        json!(before_user.as_ref().map(|u| u.allowed_models.clone())),
                    );
                    after.insert("allowed_models".into(), json!(v));
                }
                if let Some(v) = body.budget_limit {
                    before.insert(
                        "budget_limit".into(),
                        json!(before_user.as_ref().and_then(|u| u.budget_limit)),
                    );
                    after.insert("budget_limit".into(), json!(v));
                }

                let summary = if before.is_empty() {
                    format!(
                        "{} updated {} (no fields changed)",
                        admin.username, target_name
                    )
                } else {
                    format!(
                        "{} updated {} ({} field{})",
                        admin.username,
                        target_name,
                        before.len(),
                        if before.len() == 1 { "" } else { "s" }
                    )
                };

                let mut rec =
                    MutationRecord::new(&admin.username, admin.id, "user_update", summary)
                        .with_target(target_name);
                if !before.is_empty() {
                    rec = rec
                        .with_before(JsonValue::Object(before))
                        .with_after(JsonValue::Object(after));
                }
                log.record_mutation(&rec);
            }
            let next_csrf = rotate_csrf(&state, &session_id);
            let headers = csrf_next_headers(next_csrf.as_deref());
            (StatusCode::OK, headers, Json(json!({ "affected": n }))).into_response()
        }
        Err(e) => {
            tracing::warn!("update_user: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "failed to update user"})),
            )
                .into_response()
        }
    }
}

pub async fn api_force_revoke_user_tokens(
    State(state): State<Arc<ConsoleState>>,
    MutatingUser {
        user: admin,
        session_id,
    }: MutatingUser,
    Path(id): Path<i64>,
) -> Response {
    if let Err(e) = require_admin(&admin) {
        return *e;
    }

    let target = match state.db.with_conn(|conn| db::user_by_id(conn, id)) {
        Ok(Some(u)) => u,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "user not found"})),
            )
                .into_response();
        }
        Err(e) => {
            tracing::warn!("force_revoke_user_tokens: lookup failed: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal error"})),
            )
                .into_response();
        }
    };

    let revoked = match state
        .db
        .with_conn(|conn| Ok(token_store::revoke_all_for_user(conn, id)?))
    {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("force_revoke_user_tokens: revoke failed: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "failed to revoke tokens"})),
            )
                .into_response();
        }
    };

    if let Some(ref log) = state.audit_log {
        log.record_mutation(
            &MutationRecord::new(
                &admin.username,
                admin.id,
                "user_force_revoke_all",
                format!(
                    "{} force-revoked all tokens for {} ({} affected)",
                    admin.username, target.username, revoked
                ),
            )
            .with_target(target.username.clone())
            .with_after(json!({ "revoked_count": revoked })),
        );
    }

    // Same cross-process invalidation story as `api_revoke_token`:
    // a forced mass-revoke is exactly the scenario where leaving up to
    // 60s of cached "verified" results on the proxy would be a security
    // bug. See `api_revoke_token` for the design rationale.
    let reload = super::reload::trigger_invalidate_tokens(&state.config.reload).await;
    let next_csrf = rotate_csrf(&state, &session_id);
    let headers = csrf_next_headers(next_csrf.as_deref());
    (
        StatusCode::OK,
        headers,
        Json(json!({
            "user_id": id,
            "username": target.username,
            "revoked": revoked,
            "reload": {
                "triggered": reload.triggered,
                "method": reload.method,
                "error": reload.error,
            },
        })),
    )
        .into_response()
}

/// Admin-only password reset for another user. Mirrors the `nanoguard-admin
/// set-password` CLI: hash the new password, rewrite `users.password_hash`,
/// and invalidate every active session for the target — all inside one
/// transaction so a crash mid-call cannot leave a freshly reset account with
/// stale session cookies still alive. The point of resetting a password is
/// usually that the prior credential or session has been compromised; the
/// invariant the transaction protects is exactly that contract.
pub async fn api_reset_user_password(
    State(state): State<Arc<ConsoleState>>,
    MutatingUser {
        user: admin,
        session_id,
    }: MutatingUser,
    Path(id): Path<i64>,
    Json(body): Json<ResetPasswordRequest>,
) -> Response {
    if let Err(e) = require_admin(&admin) {
        return *e;
    }

    if body.password.len() < 12 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "password must be at least 12 characters"})),
        )
            .into_response();
    }
    if super::auth::is_common_password(&body.password) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "password is too common"})),
        )
            .into_response();
    }

    let target = match state.db.with_conn(|conn| db::user_by_id(conn, id)) {
        Ok(Some(u)) => u,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "user not found"})),
            )
                .into_response();
        }
        Err(e) => {
            tracing::warn!("reset_user_password: lookup failed: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal error"})),
            )
                .into_response();
        }
    };

    let hash = match super::auth::hash_password(&body.password) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("reset_user_password: hash failed: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "password hash failed"})),
            )
                .into_response();
        }
    };

    let sweep_result: anyhow::Result<()> = state.db.with_conn(|conn| {
        let tx = conn.unchecked_transaction()?;
        db::set_password_hash(&tx, target.id, &hash)?;
        db::delete_user_sessions(&tx, target.id)?;
        tx.commit()?;
        Ok(())
    });
    if let Err(e) = sweep_result {
        tracing::warn!("reset_user_password: write failed: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to reset password"})),
        )
            .into_response();
    }

    if let Some(ref log) = state.audit_log {
        log.record_mutation(
            &MutationRecord::new(
                &admin.username,
                admin.id,
                "user_password_reset",
                format!(
                    "{} reset password for {} (sessions invalidated)",
                    admin.username, target.username
                ),
            )
            .with_target(target.username.clone()),
        );
    }

    let next_csrf = rotate_csrf(&state, &session_id);
    let headers = csrf_next_headers(next_csrf.as_deref());
    (
        StatusCode::OK,
        headers,
        Json(json!({
            "user_id": target.id,
            "username": target.username,
            "sessions_invalidated": true,
        })),
    )
        .into_response()
}

// ── API: File editing (admin only) ──────────────────────────────────────────

#[derive(Deserialize)]
pub struct EditFileRequest {
    pub path: String,
    pub content: String,
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Deserialize)]
pub struct ValidateFileRequest {
    pub path: String,
    pub content: String,
}

#[derive(Deserialize)]
pub struct RevertFileRequest {
    pub path: String,
    pub backup: String,
}

#[derive(Deserialize, Default)]
pub struct BackupQuery {
    pub path: String,
}

/// Validate that a path is safe for editing/backup/revert.
fn is_safe_editable_path(path: &str) -> bool {
    let p = std::path::Path::new(path);
    // Reject absolute paths and parent directory traversal.
    if p.components().any(|c| {
        matches!(
            c,
            std::path::Component::ParentDir | std::path::Component::RootDir
        )
    }) {
        return false;
    }
    path.starts_with("dicts/") || path.starts_with("policies/") || path == "nanoguard.toml"
}

pub async fn api_edit_file(
    State(state): State<Arc<ConsoleState>>,
    MutatingUser {
        user: admin,
        session_id,
    }: MutatingUser,
    Json(body): Json<EditFileRequest>,
) -> Response {
    if let Err(e) = require_admin(&admin) {
        return *e;
    }

    let path = body.path.trim();
    if path.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "path is required"})),
        )
            .into_response();
    }

    // Security: restrict to known file families.
    if !is_safe_editable_path(path) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "editing this file is not allowed"})),
        )
            .into_response();
    }

    let validator = |content: &str| super::edit::validate_by_path(path, content);
    let backup_limit = state.config.console.backup_limit;
    let write_result = std::panic::catch_unwind(|| {
        super::edit::atomic_write_with_limit(path, &body.content, validator, backup_limit)
    });

    let (before_hash, after_hash) = match write_result {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => {
            tracing::warn!("edit_file: {}: {}", path, e);
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("{}", e)})),
            )
                .into_response();
        }
        Err(_) => {
            tracing::warn!("edit_file: panic during write");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "write failed"})),
            )
                .into_response();
        }
    };

    // Audit log. Uses the richer MutationRecord shape so file edits live in
    // the same envelope as user/token mutations — actor, actor_id, target
    // (the file path), and before/after carry the hashes.
    if let Some(ref log) = state.audit_log {
        let summary = body
            .summary
            .clone()
            .unwrap_or_else(|| format!("edited {}", path));
        log.record_mutation(
            &MutationRecord::new(&admin.username, admin.id, "edit", summary)
                .with_target(path.to_string())
                .with_before(json!({ "hash": before_hash.clone() }))
                .with_after(json!({ "hash": after_hash.clone() })),
        );
    }

    // Trigger reload.
    let reload = super::reload::trigger_reload(&state.config.reload).await;

    let next_csrf = rotate_csrf(&state, &session_id);
    let headers = csrf_next_headers(next_csrf.as_deref());
    (
        StatusCode::OK,
        headers,
        Json(json!({
            "ok": true,
            "before_hash": before_hash,
            "after_hash": after_hash,
            "reload": reload,
        })),
    )
        .into_response()
}

pub async fn api_validate_file(
    CurrentUser(admin): CurrentUser,
    Json(body): Json<ValidateFileRequest>,
) -> Response {
    if let Err(e) = require_admin(&admin) {
        return *e;
    }

    let path = body.path.trim();
    if path.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "path is required"})),
        )
            .into_response();
    }

    if !is_safe_editable_path(path) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "validating this file is not allowed"})),
        )
            .into_response();
    }

    let result = super::edit::validate_by_path(path, &body.content);
    Json(json!(result)).into_response()
}

pub async fn api_list_backups(
    CurrentUser(admin): CurrentUser,
    Query(q): Query<BackupQuery>,
) -> Response {
    if let Err(e) = require_admin(&admin) {
        return *e;
    }

    let path = q.path.trim();
    if path.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "path is required"})),
        )
            .into_response();
    }

    if !is_safe_editable_path(path) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "editing this file is not allowed"})),
        )
            .into_response();
    }

    match super::edit::list_backups(path) {
        Ok(backups) => Json(json!({ "data": backups })).into_response(),
        Err(e) => {
            tracing::warn!("list_backups: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "failed to list backups"})),
            )
                .into_response()
        }
    }
}

pub async fn api_revert_file(
    State(state): State<Arc<ConsoleState>>,
    MutatingUser {
        user: admin,
        session_id,
    }: MutatingUser,
    Json(body): Json<RevertFileRequest>,
) -> Response {
    if let Err(e) = require_admin(&admin) {
        return *e;
    }

    let path = body.path.trim();
    if path.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "path is required"})),
        )
            .into_response();
    }

    if !is_safe_editable_path(path) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "editing this file is not allowed"})),
        )
            .into_response();
    }

    let before_content = std::fs::read_to_string(path).unwrap_or_default();
    let before_hash = super::edit::hash_content(&before_content);

    let content = match super::edit::revert_to_backup(path, &body.backup) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("revert_file: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("{}", e)})),
            )
                .into_response();
        }
    };

    if let Some(ref log) = state.audit_log {
        let after_hash = super::edit::hash_content(&content);
        log.record_mutation(
            &MutationRecord::new(
                &admin.username,
                admin.id,
                "revert",
                format!("reverted {} to backup {}", path, body.backup),
            )
            .with_target(path.to_string())
            .with_before(json!({ "hash": before_hash }))
            .with_after(json!({ "hash": after_hash, "backup": body.backup.clone() })),
        );
    }

    let reload = super::reload::trigger_reload(&state.config.reload).await;

    let next_csrf = rotate_csrf(&state, &session_id);
    let headers = csrf_next_headers(next_csrf.as_deref());
    (
        StatusCode::OK,
        headers,
        Json(json!({
            "ok": true,
            "reload": reload,
        })),
    )
        .into_response()
}

// ── API: Reload ─────────────────────────────────────────────────────────────

pub async fn api_trigger_reload(
    State(state): State<Arc<ConsoleState>>,
    MutatingUser {
        user: admin,
        session_id,
    }: MutatingUser,
) -> Response {
    if let Err(e) = require_admin(&admin) {
        return *e;
    }

    let outcome = super::reload::trigger_reload(&state.config.reload).await;
    let next_csrf = rotate_csrf(&state, &session_id);
    let headers = csrf_next_headers(next_csrf.as_deref());
    (StatusCode::OK, headers, Json(json!(outcome))).into_response()
}

#[derive(Deserialize, Default)]
pub struct ReloadStatusQuery {
    #[serde(default)]
    pub since: Option<f64>,
}

pub async fn api_reload_status(
    State(state): State<Arc<ConsoleState>>,
    CurrentUser(admin): CurrentUser,
    Query(q): Query<ReloadStatusQuery>,
) -> Response {
    if let Err(e) = require_admin(&admin) {
        return *e;
    }

    let since = q.since.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
            - 30.0
    });
    if !since.is_finite() || since < 0.0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "since must be a non-negative finite number"})),
        )
            .into_response();
    }
    let after = std::time::UNIX_EPOCH + std::time::Duration::from_secs_f64(since);
    let result = super::reload::poll_reload_status(&state.config.audit.path, after, 200);
    match result {
        Some(ok) => Json(json!({
            "ready": true,
            "ok": ok,
            "error": null,
        }))
        .into_response(),
        None => Json(json!({
            "ready": false,
            "ok": null,
            "error": null,
        }))
        .into_response(),
    }
}

// ── API: Console audit log ──────────────────────────────────────────────────

pub async fn api_console_audit(
    State(state): State<Arc<ConsoleState>>,
    CurrentUser(user): CurrentUser,
    Query(q): Query<ConsoleAuditQuery>,
) -> Response {
    if let Err(e) = require_admin(&user) {
        return *e;
    }

    let path = &state.config.console.audit_path;
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                return Json(json!({ "data": [] })).into_response();
            }
            tracing::warn!("console_audit: read failed: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "failed to read console audit log"})),
            )
                .into_response();
        }
    };

    let entries = filter_console_audit_entries(
        &content,
        q.action.as_deref().or(q.verdict.as_deref()),
        q.actor.as_deref(),
        q.target.as_deref(),
        q.limit.unwrap_or(100),
    );
    Json(json!({ "data": entries })).into_response()
}

/// Walk the console audit JSONL tail-first and return up to `limit` entries
/// matching every supplied filter. Pure over the file content so it can be
/// unit-tested without a live filesystem or HTTP request.
///
/// `verdict` is kept as an alias for `action` for backward compat with the
/// original Phase 1 viewer query string.
pub(crate) fn filter_console_audit_entries(
    content: &str,
    action: Option<&str>,
    actor: Option<&str>,
    target: Option<&str>,
    limit: usize,
) -> Vec<JsonValue> {
    let mut entries = Vec::new();

    // When a narrow filter is set we cannot bound the scan by `limit * 2`
    // — the matching entries might all live further back in the file.
    let scan_lines: Vec<&str> = content.lines().rev().collect();
    let cap = if action.is_some() || actor.is_some() || target.is_some() {
        scan_lines.len()
    } else {
        (limit.saturating_mul(2)).min(scan_lines.len())
    };

    for line in scan_lines.into_iter().take(cap) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<JsonValue>(line) else {
            continue;
        };

        if let Some(v) = action {
            if obj.get("action").and_then(|x| x.as_str()) != Some(v) {
                continue;
            }
        }
        if let Some(v) = actor {
            if obj.get("actor").and_then(|x| x.as_str()) != Some(v) {
                continue;
            }
        }
        if let Some(v) = target {
            // The legacy EditRecord shape used `file` instead of `target` for
            // file edits. Match either so a single filter covers both shapes.
            let matches = obj.get("target").and_then(|x| x.as_str()) == Some(v)
                || obj.get("file").and_then(|x| x.as_str()) == Some(v);
            if !matches {
                continue;
            }
        }

        entries.push(obj);
        if entries.len() >= limit {
            break;
        }
    }

    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(action: &str, actor: &str, target: &str) -> String {
        serde_json::to_string(&json!({
            "request_id": "deadbeef",
            "timestamp": "2026-05-17T00:00:00Z",
            "actor": actor,
            "actor_id": "1",
            "action": action,
            "target": target,
            "summary": format!("{actor} {action} {target}"),
        }))
        .unwrap()
    }

    fn legacy_edit_line(file: &str, actor: &str) -> String {
        serde_json::to_string(&json!({
            "timestamp": "2026-05-17T00:00:00Z",
            "actor": actor,
            "action": "edit",
            "file": file,
            "before_hash": "aa",
            "after_hash": "bb",
            "summary": "legacy edit",
        }))
        .unwrap()
    }

    #[test]
    fn filter_returns_tail_first_without_filters() {
        let content = format!(
            "{}\n{}\n{}\n",
            line("login", "alice", "alice"),
            line("user_create", "alice", "carol"),
            line("logout", "alice", "alice"),
        );
        let out = filter_console_audit_entries(&content, None, None, None, 10);
        // Tail-first: newest entry (logout) is first.
        assert_eq!(out.len(), 3);
        assert_eq!(out[0]["action"], "logout");
        assert_eq!(out[2]["action"], "login");
    }

    #[test]
    fn filter_by_actor_returns_only_that_actor() {
        let content = format!(
            "{}\n{}\n{}\n",
            line("login", "alice", "alice"),
            line("login", "bob", "bob"),
            line("logout", "alice", "alice"),
        );
        let out = filter_console_audit_entries(&content, None, Some("alice"), None, 10);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|e| e["actor"] == "alice"));
    }

    #[test]
    fn filter_by_target_matches_target_or_legacy_file_field() {
        let content = format!(
            "{}\n{}\n{}\n",
            line("user_update", "alice", "carol"),
            legacy_edit_line("dicts/test.txt", "alice"),
            line("user_update", "alice", "dave"),
        );

        let by_target = filter_console_audit_entries(&content, None, None, Some("carol"), 10);
        assert_eq!(by_target.len(), 1);
        assert_eq!(by_target[0]["target"], "carol");

        // The same `target` filter also pulls the legacy file-edit record so
        // operators don't have to know about the on-disk schema migration.
        let by_file =
            filter_console_audit_entries(&content, None, None, Some("dicts/test.txt"), 10);
        assert_eq!(by_file.len(), 1);
        assert_eq!(by_file[0]["file"], "dicts/test.txt");
    }

    #[test]
    fn filter_by_action_and_actor_combines_with_and_semantics() {
        let content = format!(
            "{}\n{}\n{}\n",
            line("login", "alice", "alice"),
            line("token_create", "alice", "alice"),
            line("token_create", "bob", "bob"),
        );
        let out =
            filter_console_audit_entries(&content, Some("token_create"), Some("alice"), None, 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["actor"], "alice");
        assert_eq!(out[0]["action"], "token_create");
    }

    #[test]
    fn filter_respects_limit_and_skips_blank_and_malformed_lines() {
        let mut s = String::new();
        for i in 0..50 {
            s.push_str(&line("login", "alice", &format!("u{i}")));
            s.push('\n');
            s.push('\n'); // blank line — should be skipped
            s.push_str("{not-json}\n"); // malformed — should be skipped
        }
        let out = filter_console_audit_entries(&s, Some("login"), None, None, 5);
        assert_eq!(out.len(), 5);
        // Tail-first ordering: the newest login (u49) shows up first.
        assert_eq!(out[0]["target"], "u49");
    }
}
