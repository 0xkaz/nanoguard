//! HTTP handlers for the nanoguard console.

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use rusqlite::{params, OptionalExtension};
use std::collections::HashMap;
use std::sync::Arc;

use crate::client_auth::{store as token_store, Token};

use super::{
    auth::{build_logout_cookie, build_session_cookie, verify_password, CurrentUser, MaybeUser},
    db::{self, User},
    ConsoleState,
};

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
            created_at: u.created_at,
            last_login_at: u.last_login_at,
        }
    }
}

// ── Auth helpers ────────────────────────────────────────────────────────────

fn require_admin(user: &User) -> Result<(), Response> {
    if user.role != "admin" {
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({"error": "admin required"})),
        )
            .into_response());
    }
    Ok(())
}

fn session_cookie(state: &ConsoleState, session_id: &[u8]) -> cookie::Cookie<'static> {
    build_session_cookie(session_id, &state.config.console.session_secret, state.secure_cookie)
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
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid credentials"})),
        )
            .into_response();
    };

    if user.disabled {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "account disabled"})),
        )
            .into_response();
    }

    let Some(ref hash) = user.password_hash else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid credentials"})),
        )
            .into_response();
    };

    if !verify_password(&body.password, hash) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid credentials"})),
        )
            .into_response();
    }

    if let Err(e) = state.db.with_conn(|conn| db::update_last_login(conn, user.id)) {
        tracing::warn!("login: failed to update last_login: {}", e);
    }

    let session_id = super::auth::generate_session_id();
    let ttl_hours = state.config.console.session_ttl_hours;
    let expires = chrono::Utc::now() + chrono::Duration::hours(ttl_hours);
    let user_agent = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok());
    let ip = headers
        .get("x-forwarded-for")
        .or_else(|| headers.get("x-real-ip"))
        .and_then(|v| v.to_str().ok());

    if let Err(e) = state.db.with_conn(|conn| {
        db::create_session(conn, &session_id, user.id, &expires.to_rfc3339(), user_agent, ip)
    }) {
        tracing::warn!("login: failed to create session: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "session creation failed"})),
        )
            .into_response();
    }

    let cookie = session_cookie(&state, &session_id);
    (
        StatusCode::OK,
        [(axum::http::header::SET_COOKIE, cookie.to_string())],
        Json(json!({"ok": true, "user": UserResponse::from(user)})),
    )
        .into_response()
}

pub async fn api_logout(
    State(state): State<Arc<ConsoleState>>,
    MaybeUser(user): MaybeUser,
) -> Response {
    // If we can extract the session, delete it from the DB.
    // We do this by parsing the cookie manually since axum doesn't give us
    // the raw cookie value in a convenient way here.
    // Instead, we'll just let the client clear it and rely on the cookie expiry.
    if let Some(u) = user {
        // Best-effort: delete all sessions for this user (aggressive logout).
        let _ = state.db.with_conn(|conn| db::delete_user_sessions(conn, u.id));
    }

    let cookie = logout_cookie(&state);
    (
        StatusCode::OK,
        [(axum::http::header::SET_COOKIE, cookie.to_string())],
        Json(json!({"ok": true})),
    )
        .into_response()
}

pub async fn api_me(CurrentUser(user): CurrentUser) -> impl IntoResponse {
    Json(UserResponse::from(user))
}

// ── API: Tokens (self-service) ──────────────────────────────────────────────

pub async fn api_list_tokens(
    State(state): State<Arc<ConsoleState>>,
    CurrentUser(user): CurrentUser,
) -> Response {
    let result = state.db.with_conn(|conn| Ok(token_store::list_for_user(conn, user.id)?));
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
    CurrentUser(user): CurrentUser,
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

    // Use the proxy's env marker from config. Default to 'p'.
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
            tracing::warn!("create_token: failed after {} attempts: {:?}", MAX_ATTEMPTS, last_err);
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
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Response {
    // Verify the token belongs to the current user (non-admins cannot revoke others' tokens).
    let belongs = state
        .db
        .with_conn(|conn| {
            let rows = token_store::list_for_user(conn, user.id)?;
            Ok(rows.into_iter().any(|r| r.id == id))
        });

    match belongs {
        Ok(true) => {}
        Ok(false) => {
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
    }

    match state.db.with_conn(|conn| Ok(token_store::revoke(conn, id)?)) {
        Ok(affected) => Json(json!({
            "id": id,
            "status": "revoked",
            "affected": affected,
        }))
        .into_response(),
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

        // Filter by verdict if requested.
        if let Some(ref v) = q.verdict {
            if obj.get("verdict").and_then(|x| x.as_str()) != Some(v) {
                continue;
            }
        }

        // Non-admins only see entries where user_id matches their own id.
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

    // Open a read-only connection for budget queries.
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
        // Admin sees all api_keys with usage / limit.
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
        // User sees their own token-based budgets.
        // First get their token prefixes.
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

    // nanoguard.toml
    if let Ok(content) = std::fs::read_to_string("nanoguard.toml") {
        files.insert("nanoguard.toml".to_string(), content);
    }

    // dicts
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

    // policies
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
        return e;
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
    CurrentUser(admin): CurrentUser,
    Json(body): Json<CreateUserRequest>,
) -> Response {
    if let Err(e) = require_admin(&admin) {
        return e;
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
        Ok(id) => (
            StatusCode::CREATED,
            Json(json!({
                "id": id,
                "username": username,
                "role": role,
            })),
        )
            .into_response(),
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
    CurrentUser(admin): CurrentUser,
    Path(id): Path<i64>,
    Json(body): Json<UpdateUserRequest>,
) -> Response {
    if let Err(e) = require_admin(&admin) {
        return e;
    }

    match state.db.with_conn(|conn| {
        db::update_user(
            conn,
            id,
            body.display_name.as_deref(),
            body.email.as_deref(),
            body.role.as_deref(),
            body.disabled,
            body.allowed_models.as_deref(),
            body.budget_limit,
        )
    }) {
        Ok(n) => Json(json!({ "affected": n })).into_response(),
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
