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
        let _ = state.db.with_conn(|conn| {
            db::record_login_attempt(conn, &body.username, ip)
        });
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
        let _ = state.db.with_conn(|conn| db::record_login_attempt(conn, &user.username, ip));
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid credentials"})),
        )
            .into_response();
    };

    if !verify_password(&body.password, hash) {
        let _ = state.db.with_conn(|conn| {
            db::record_login_attempt(conn, &user.username, ip)
        });
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
            let until = (chrono::Utc::now() + chrono::Duration::minutes(lockout_minutes))
                .to_rfc3339();
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

    // Rate-limit per IP
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
            let next_csrf = rotate_csrf(&state, &session_id);
            let headers = csrf_next_headers(next_csrf.as_deref());
            (
                StatusCode::OK,
                headers,
                Json(json!({
                    "id": id,
                    "status": "revoked",
                    "affected": affected,
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

    let next_csrf = rotate_csrf(&state, &session_id);
    let headers = csrf_next_headers(next_csrf.as_deref());
    (
        StatusCode::OK,
        headers,
        Json(json!({
            "user_id": id,
            "username": target.username,
            "revoked": revoked,
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
    let reload = super::reload::trigger_reload(&state.config.reload);

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

    let reload = super::reload::trigger_reload(&state.config.reload);

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

    let outcome = super::reload::trigger_reload(&state.config.reload);
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
