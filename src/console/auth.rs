//! Authentication for the nanoguard console.
//!
//! Local-password auth with argon2id + signed session cookies.

use argon2::{
    password_hash::{
        rand_core::RngCore, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
    },
    Argon2,
};
use axum::{
    async_trait,
    extract::FromRequestParts,
    http::{request::Parts, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use cookie::{Cookie, Key, SameSite};

/// Small hardcoded set of common passwords that must be rejected even if
/// they meet the length requirement. This is a lightweight defense; larger
/// deployments should use a full dictionary (e.g. HIBP) via external check.
const COMMON_PASSWORDS: &[&str] = &[
    "password123456",
    "123456789012",
    "qwertyuiop[]",
    "letmein1234567",
    "welcome1234567",
    "adminadminadmin",
    "nanoguard12345",
];
use rand::rngs::OsRng;
use std::sync::Arc;
use subtle::ConstantTimeEq;

use super::db::{session_by_id, user_by_id, User};

/// HTTP header carrying the per-session CSRF token on mutating requests.
pub const CSRF_HEADER: &str = "x-csrf-token";

/// HTTP response header used to communicate a rotated CSRF token back to the
/// client after a successful mutation. JS clients refresh their cached token
/// from this header.
pub const CSRF_NEXT_HEADER: &str = "x-csrf-token-next";

#[derive(Debug)]
pub struct AuthError(pub StatusCode, pub String);

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({"error": self.1 });
        (self.0, axum::Json(body)).into_response()
    }
}

/// Hashes a password with argon2id. Returns the encoded hash string as bytes.
pub fn hash_password(password: &str) -> anyhow::Result<Vec<u8>> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let password_hash = argon2
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("argon2 hash failed: {}", e))?;
    Ok(password_hash.to_string().into_bytes())
}

/// Verifies a password against a stored argon2 hash.
pub fn verify_password(password: &str, hash_bytes: &[u8]) -> bool {
    let hash_str = match std::str::from_utf8(hash_bytes) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let parsed_hash = match PasswordHash::new(hash_str) {
        Ok(h) => h,
        Err(_) => return false,
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed_hash)
        .is_ok()
}

/// Generate a 32-byte random session ID.
pub fn generate_session_id() -> Vec<u8> {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.to_vec()
}

/// Generate a 32-byte random CSRF token. Returned as raw bytes; encode with
/// [`encode_csrf_token`] before sending to a client.
pub fn generate_csrf_token() -> Vec<u8> {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.to_vec()
}

/// Encode a raw CSRF token for wire transport. Hex keeps the value
/// header-safe and trivial to compare client-side.
pub fn encode_csrf_token(raw: &[u8]) -> String {
    hex::encode(raw)
}

/// Decode an incoming CSRF token from the header back into raw bytes. Returns
/// `None` on malformed input rather than propagating an error so the handler
/// can fold "missing" and "garbage" into a single 403.
pub fn decode_csrf_token(s: &str) -> Option<Vec<u8>> {
    hex::decode(s.trim()).ok()
}

/// Constant-time comparison of two CSRF tokens. Both inputs are byte slices
/// of the raw 32-byte token (already hex-decoded). Defeats timing oracles
/// that would otherwise reveal the prefix of the stored token.
pub fn csrf_tokens_match(provided: &[u8], stored: &[u8]) -> bool {
    if provided.len() != stored.len() {
        return false;
    }
    provided.ct_eq(stored).into()
}

/// Build a signed session cookie.
pub fn build_session_cookie(session_id: &[u8], secret: &str, secure: bool) -> Cookie<'static> {
    let key = Key::derive_from(secret.as_bytes());
    let value = hex::encode(session_id);
    let mut cookie = Cookie::new("ng_session", value.clone());
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Strict);
    cookie.set_path("/");
    cookie.set_secure(secure);

    let mut jar = cookie::CookieJar::new();
    jar.signed_mut(&key).add(cookie.clone());
    // Return the cookie from the jar so it carries the signed value.
    jar.get("ng_session")
        .cloned()
        .unwrap_or_else(|| Cookie::new("ng_session", value))
}

/// Build a cookie that clears the session.
pub fn build_logout_cookie(secret: &str, secure: bool) -> Cookie<'static> {
    let key = Key::derive_from(secret.as_bytes());
    let mut cookie = Cookie::new("ng_session", "");
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Strict);
    cookie.set_path("/");
    cookie.set_secure(secure);
    cookie.set_max_age(cookie::time::Duration::seconds(0));

    let mut jar = cookie::CookieJar::new();
    jar.signed_mut(&key).add(cookie.clone());
    jar.get("ng_session").cloned().unwrap_or_else(|| {
        let mut c = Cookie::new("ng_session", "");
        c.set_max_age(cookie::time::Duration::seconds(0));
        c
    })
}

/// Extract the raw session ID from the signed cookie header value.
pub fn extract_session_id(headers: &HeaderMap, secret: &str) -> Option<Vec<u8>> {
    let key = Key::derive_from(secret.as_bytes());
    let mut jar = cookie::CookieJar::new();

    let cookie_header = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for cookie_str in cookie_header.split(';') {
        let cookie_str = cookie_str.trim();
        if let Ok(c) = Cookie::parse(cookie_str) {
            jar.add_original(c.into_owned());
        }
    }

    let session_cookie = jar.signed(&key).get("ng_session")?;
    hex::decode(session_cookie.value()).ok()
}

/// Check whether a password appears in the small built-in common-password
/// list. Case-insensitive comparison.
pub fn is_common_password(password: &str) -> bool {
    let lower = password.to_lowercase();
    COMMON_PASSWORDS.iter().any(|&p| p.to_lowercase() == lower)
}

/// Current user extractor for axum handlers.
#[derive(Clone, Debug)]
pub struct CurrentUser(pub User);

#[async_trait]
impl FromRequestParts<Arc<super::ConsoleState>> for CurrentUser {
    type Rejection = AuthError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<super::ConsoleState>,
    ) -> Result<Self, Self::Rejection> {
        let session_id = extract_session_id(&parts.headers, &state.config.console.session_secret)
            .ok_or_else(|| AuthError(StatusCode::UNAUTHORIZED, "no session".into()))?;

        let idle_timeout_hours = state
            .config
            .console
            .session_idle_timeout_hours
            .unwrap_or(state.config.console.session_ttl_hours);

        let (user, expired) = state
            .db
            .with_conn(|conn| {
                let session = super::db::session_by_id(conn, &session_id)?;
                let Some(session) = session else {
                    return Ok((None, true));
                };
                let now = chrono::Utc::now();
                let expires = chrono::DateTime::parse_from_rfc3339(&session.expires_at)
                    .unwrap_or_else(|_| chrono::DateTime::UNIX_EPOCH.into());
                if now > expires {
                    let _ = super::db::delete_session(conn, &session_id);
                    return Ok((None, true));
                }
                // Idle-timeout check
                let last_seen = chrono::DateTime::parse_from_rfc3339(&session.last_seen_at)
                    .unwrap_or_else(|_| chrono::DateTime::UNIX_EPOCH.into());
                let idle_cutoff = now - chrono::Duration::hours(idle_timeout_hours);
                if last_seen < idle_cutoff {
                    let _ = super::db::delete_session(conn, &session_id);
                    return Ok((None, true));
                }
                let _ = super::db::touch_session(conn, &session_id);
                let user = user_by_id(conn, session.user_id)?;
                Ok((user, false))
            })
            .map_err(|e| AuthError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        if expired {
            return Err(AuthError(
                StatusCode::UNAUTHORIZED,
                "session expired".into(),
            ));
        }

        let Some(user) = user else {
            return Err(AuthError(StatusCode::UNAUTHORIZED, "user not found".into()));
        };

        if user.disabled {
            return Err(AuthError(
                StatusCode::UNAUTHORIZED,
                "account disabled".into(),
            ));
        }

        Ok(CurrentUser(user))
    }
}

/// Extractor for mutating handlers: combines session lookup with CSRF
/// double-submit verification. The handler must call
/// [`super::db::rotate_csrf_token`] after a successful side-effecting write
/// and include the new token in the response via [`CSRF_NEXT_HEADER`].
///
/// The flow is:
///
/// 1. Extract the session ID from the signed cookie (same as `CurrentUser`).
/// 2. Look up the session row, including its stored `csrf_token`.
/// 3. Compare the `X-CSRF-Token` header against the stored token in constant
///    time. Missing header, malformed hex, mismatched length, or mismatched
///    bytes all collapse to a single `403 Forbidden` to avoid leaking which
///    failure mode tripped.
/// 4. Hand the user + session id to the handler.
#[derive(Debug)]
pub struct MutatingUser {
    pub user: User,
    pub session_id: Vec<u8>,
}

#[async_trait]
impl FromRequestParts<Arc<super::ConsoleState>> for MutatingUser {
    type Rejection = AuthError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<super::ConsoleState>,
    ) -> Result<Self, Self::Rejection> {
        let session_id = extract_session_id(&parts.headers, &state.config.console.session_secret)
            .ok_or_else(|| AuthError(StatusCode::UNAUTHORIZED, "no session".into()))?;

        let provided_header = parts
            .headers
            .get(CSRF_HEADER)
            .and_then(|v| v.to_str().ok())
            .and_then(decode_csrf_token);

        let idle_timeout_hours = state
            .config
            .console
            .session_idle_timeout_hours
            .unwrap_or(state.config.console.session_ttl_hours);

        let (user, session_csrf, expired) = state
            .db
            .with_conn(|conn| {
                let session = session_by_id(conn, &session_id)?;
                let Some(session) = session else {
                    return Ok((None, None, true));
                };
                let now = chrono::Utc::now();
                let expires = chrono::DateTime::parse_from_rfc3339(&session.expires_at)
                    .unwrap_or_else(|_| chrono::DateTime::UNIX_EPOCH.into());
                if now > expires {
                    let _ = super::db::delete_session(conn, &session_id);
                    return Ok((None, None, true));
                }
                // Idle-timeout check
                let last_seen = chrono::DateTime::parse_from_rfc3339(&session.last_seen_at)
                    .unwrap_or_else(|_| chrono::DateTime::UNIX_EPOCH.into());
                let idle_cutoff = now - chrono::Duration::hours(idle_timeout_hours);
                if last_seen < idle_cutoff {
                    let _ = super::db::delete_session(conn, &session_id);
                    return Ok((None, None, true));
                }
                let _ = super::db::touch_session(conn, &session_id);
                let user = user_by_id(conn, session.user_id)?;
                Ok((user, session.csrf_token, false))
            })
            .map_err(|e| AuthError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        if expired {
            return Err(AuthError(
                StatusCode::UNAUTHORIZED,
                "session expired".into(),
            ));
        }

        let Some(user) = user else {
            return Err(AuthError(StatusCode::UNAUTHORIZED, "user not found".into()));
        };

        if user.disabled {
            return Err(AuthError(
                StatusCode::UNAUTHORIZED,
                "account disabled".into(),
            ));
        }

        let stored = session_csrf.ok_or_else(|| {
            AuthError(
                StatusCode::FORBIDDEN,
                "csrf token missing from session".into(),
            )
        })?;
        let provided = provided_header
            .ok_or_else(|| AuthError(StatusCode::FORBIDDEN, "csrf token missing".into()))?;
        if !csrf_tokens_match(&provided, &stored) {
            return Err(AuthError(
                StatusCode::FORBIDDEN,
                "csrf token mismatch".into(),
            ));
        }

        Ok(MutatingUser { user, session_id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_and_verify() {
        let hash = hash_password("correct-horse-battery-staple-42").unwrap();
        assert!(verify_password("correct-horse-battery-staple-42", &hash));
        assert!(!verify_password("wrong-password", &hash));
    }

    #[test]
    fn session_id_is_32_bytes() {
        let id = generate_session_id();
        assert_eq!(id.len(), 32);
    }

    #[test]
    fn csrf_token_roundtrip() {
        let raw = generate_csrf_token();
        assert_eq!(raw.len(), 32);
        let wire = encode_csrf_token(&raw);
        let back = decode_csrf_token(&wire).expect("decodes");
        assert_eq!(raw, back);
    }

    #[test]
    fn csrf_tokens_match_accepts_equal_and_rejects_diffs() {
        let a = vec![7u8; 32];
        let b = vec![7u8; 32];
        let c = vec![8u8; 32];
        let short = vec![7u8; 16];
        assert!(csrf_tokens_match(&a, &b));
        assert!(!csrf_tokens_match(&a, &c));
        assert!(!csrf_tokens_match(&a, &short));
    }

    #[test]
    fn csrf_decode_rejects_garbage() {
        assert!(decode_csrf_token("not-hex").is_none());
        assert!(decode_csrf_token("").is_some()); // empty hex decodes to empty
    }

    #[test]
    fn session_cookie_uses_samesite_strict() {
        let sid = generate_session_id();
        let cookie = build_session_cookie(&sid, "test-secret-at-least-32-bytes-long!!!", false);
        assert_eq!(cookie.same_site(), Some(SameSite::Strict));
        assert_eq!(cookie.http_only(), Some(true));
    }

    #[test]
    fn logout_cookie_uses_samesite_strict() {
        let cookie = build_logout_cookie("test-secret-at-least-32-bytes-long!!!", false);
        assert_eq!(cookie.same_site(), Some(SameSite::Strict));
        assert_eq!(cookie.http_only(), Some(true));
    }

    #[test]
    fn common_password_rejects_known_weak_passwords() {
        assert!(is_common_password("password123456"));
        assert!(is_common_password("PASSWORD123456"));
        assert!(!is_common_password("uncommon-horse-battery-99"));
    }

    // ── MutatingUser extractor integration tests ─────────────────────────
    //
    // The extractor combines session lookup with CSRF verification. The
    // tests below stand up a real `ConsoleState` against an in-memory
    // SQLite DB and drive it via `from_request_parts` to cover the missing
    // / mismatched / valid + rotation paths called out in XKA-59.

    use crate::console::db::ConsoleDb;
    use crate::console::ConsoleState;
    use axum::http::Request;
    use rusqlite::params;
    use std::sync::{Arc, Mutex};

    fn test_state() -> (Arc<ConsoleState>, i64, Vec<u8>, Vec<u8>) {
        let mut config = crate::config::Config::from_file_content(
            r#"
[backend]
provider = "ollama"
endpoint = "http://localhost:11434"

[console]
session_secret = "test-secret-for-csrf-tests-zzzzz"
"#,
        )
        .expect("parse test config");
        // Force in-memory budget DB so the ConsoleDb opens against ":memory:".
        config.budget.db_path = ":memory:".to_string();

        let db = ConsoleDb {
            conn: Mutex::new(rusqlite::Connection::open_in_memory().unwrap()),
        };
        db.with_conn(crate::console::db::__test_migrate).unwrap();

        let user_id = db
            .with_conn(|c| {
                crate::console::db::insert_user(
                    c,
                    "alice",
                    Some("Alice"),
                    None,
                    "admin",
                    Some(b"fake-hash"),
                )
            })
            .unwrap();

        let sid = generate_session_id();
        let csrf = generate_csrf_token();
        db.with_conn(|c| {
            crate::console::db::create_session(
                c,
                &sid,
                user_id,
                "2099-12-31T23:59:59Z",
                None,
                None,
                &csrf,
            )
        })
        .unwrap();

        let state = Arc::new(ConsoleState {
            config,
            db,
            secure_cookie: false,
            audit_log: None,
        });
        (state, user_id, sid, csrf)
    }

    fn cookie_header(state: &ConsoleState, sid: &[u8]) -> String {
        let cookie = build_session_cookie(sid, &state.config.console.session_secret, false);
        cookie.to_string()
    }

    #[tokio::test]
    async fn mutating_user_rejects_missing_csrf_header() {
        let (state, _uid, sid, _csrf) = test_state();
        let req = Request::builder()
            .uri("/api/tokens")
            .header(axum::http::header::COOKIE, cookie_header(&state, &sid))
            .body(())
            .unwrap();
        let (mut parts, _) = req.into_parts();
        let err = MutatingUser::from_request_parts(&mut parts, &state)
            .await
            .expect_err("missing csrf must reject");
        assert_eq!(err.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn mutating_user_rejects_mismatched_csrf() {
        let (state, _uid, sid, _csrf) = test_state();
        let wrong = encode_csrf_token(&[0xAAu8; 32]);
        let req = Request::builder()
            .uri("/api/tokens")
            .header(axum::http::header::COOKIE, cookie_header(&state, &sid))
            .header(CSRF_HEADER, wrong)
            .body(())
            .unwrap();
        let (mut parts, _) = req.into_parts();
        let err = MutatingUser::from_request_parts(&mut parts, &state)
            .await
            .expect_err("mismatched csrf must reject");
        assert_eq!(err.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn mutating_user_accepts_valid_csrf() {
        let (state, uid, sid, csrf) = test_state();
        let req = Request::builder()
            .uri("/api/tokens")
            .header(axum::http::header::COOKIE, cookie_header(&state, &sid))
            .header(CSRF_HEADER, encode_csrf_token(&csrf))
            .body(())
            .unwrap();
        let (mut parts, _) = req.into_parts();
        let mu = MutatingUser::from_request_parts(&mut parts, &state)
            .await
            .expect("valid csrf must pass");
        assert_eq!(mu.user.id, uid);
        assert_eq!(mu.session_id, sid);
    }

    #[tokio::test]
    async fn rotate_csrf_token_changes_stored_value() {
        let (state, _uid, sid, csrf) = test_state();
        let new_raw = generate_csrf_token();
        let n = state
            .db
            .with_conn(|c| crate::console::db::rotate_csrf_token(c, &sid, &new_raw))
            .unwrap();
        assert_eq!(n, 1);
        let session = state
            .db
            .with_conn(|c| crate::console::db::session_by_id(c, &sid))
            .unwrap()
            .unwrap();
        assert_eq!(session.csrf_token.as_deref(), Some(new_raw.as_slice()));
        assert_ne!(session.csrf_token.as_deref(), Some(csrf.as_slice()));

        // Old token now fails the extractor; new token succeeds.
        let req_old = Request::builder()
            .uri("/api/tokens")
            .header(axum::http::header::COOKIE, cookie_header(&state, &sid))
            .header(CSRF_HEADER, encode_csrf_token(&csrf))
            .body(())
            .unwrap();
        let (mut parts, _) = req_old.into_parts();
        let err = MutatingUser::from_request_parts(&mut parts, &state)
            .await
            .expect_err("old csrf must reject after rotation");
        assert_eq!(err.0, StatusCode::FORBIDDEN);

        let req_new = Request::builder()
            .uri("/api/tokens")
            .header(axum::http::header::COOKIE, cookie_header(&state, &sid))
            .header(CSRF_HEADER, encode_csrf_token(&new_raw))
            .body(())
            .unwrap();
        let (mut parts, _) = req_new.into_parts();
        MutatingUser::from_request_parts(&mut parts, &state)
            .await
            .expect("new csrf must pass after rotation");
    }

    #[tokio::test]
    async fn current_user_rejects_idle_session() {
        let mut config = crate::config::Config::from_file_content(
            r#"
[backend]
provider = "ollama"
endpoint = "http://localhost:11434"

[console]
session_secret = "test-secret-for-idle-tests-zzzzz"
session_idle_timeout_hours = 1
"#,
        )
        .expect("parse test config");
        config.budget.db_path = ":memory:".to_string();

        let db = crate::console::db::ConsoleDb {
            conn: std::sync::Mutex::new(rusqlite::Connection::open_in_memory().unwrap()),
        };
        db.with_conn(crate::console::db::__test_migrate).unwrap();

        let user_id = db
            .with_conn(|c| {
                crate::console::db::insert_user(c, "idle_alice", None, None, "user", None)
            })
            .unwrap();

        let sid = generate_session_id();
        let csrf = generate_csrf_token();
        // Session last_seen 2 hours ago → idle
        let old_last_seen =
            (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        let expires = (chrono::Utc::now() + chrono::Duration::hours(24)).to_rfc3339();
        db.with_conn(|c| {
            c.execute(
                "INSERT INTO user_sessions (id, user_id, created_at, expires_at, last_seen_at, user_agent, ip, csrf_token)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![&sid, user_id, old_last_seen.clone(), expires, old_last_seen, None::<&str>, None::<&str>, &csrf],
            )?;
            Ok(())
        })
        .unwrap();

        let state = std::sync::Arc::new(crate::console::ConsoleState {
            config,
            db,
            secure_cookie: false,
            audit_log: None,
        });

        let req = Request::builder()
            .uri("/api/me")
            .header(axum::http::header::COOKIE, cookie_header(&state, &sid))
            .body(())
            .unwrap();
        let (mut parts, _) = req.into_parts();
        let err = CurrentUser::from_request_parts(&mut parts, &state)
            .await
            .expect_err("idle session must reject");
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }
}
