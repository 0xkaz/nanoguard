//! Authentication for the nanoguard console.
//!
//! Local-password auth with argon2id + signed session cookies.

use argon2::{
    password_hash::{rand_core::RngCore, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::{
    async_trait,
    extract::FromRequestParts,
    http::{request::Parts, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use cookie::{Cookie, Key, SameSite};
use rand::rngs::OsRng;
use std::sync::Arc;

use super::db::{user_by_id, User};

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

/// Build a signed session cookie.
pub fn build_session_cookie(session_id: &[u8], secret: &str, secure: bool) -> Cookie<'static> {
    let key = Key::derive_from(secret.as_bytes());
    let value = hex::encode(session_id);
    let mut cookie = Cookie::new("ng_session", value.clone());
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Lax);
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
    cookie.set_same_site(SameSite::Lax);
    cookie.set_path("/");
    cookie.set_secure(secure);
    cookie.set_max_age(cookie::time::Duration::seconds(0));

    let mut jar = cookie::CookieJar::new();
    jar.signed_mut(&key).add(cookie.clone());
    jar.get("ng_session")
        .cloned()
        .unwrap_or_else(|| {
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
                let _ = super::db::touch_session(conn, &session_id);
                let user = user_by_id(conn, session.user_id)?;
                Ok((user, false))
            })
            .map_err(|e| AuthError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        if expired {
            return Err(AuthError(StatusCode::UNAUTHORIZED, "session expired".into()));
        }

        let Some(user) = user else {
            return Err(AuthError(StatusCode::UNAUTHORIZED, "user not found".into()));
        };

        if user.disabled {
            return Err(AuthError(StatusCode::UNAUTHORIZED, "account disabled".into()));
        }

        Ok(CurrentUser(user))
    }
}

/// Optional current user extractor (returns None for anonymous requests).
pub struct MaybeUser(pub Option<User>);

#[async_trait]
impl FromRequestParts<Arc<super::ConsoleState>> for MaybeUser {
    type Rejection = AuthError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<super::ConsoleState>,
    ) -> Result<Self, Self::Rejection> {
        match CurrentUser::from_request_parts(parts, state).await {
            Ok(CurrentUser(u)) => Ok(MaybeUser(Some(u))),
            Err(_) => Ok(MaybeUser(None)),
        }
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
}
