//! Axum middleware that verifies a client Bearer token before a request
//! reaches the proxy handlers.
//!
//! Sits on the route layer, NOT inside each handler. Skipped entirely
//! when `[auth].enabled = false` so Stage 1 deployments remain
//! backwards-compatible.
//!
//! On success, attaches a [`ClientView`] to the request's extensions —
//! downstream handlers retrieve it with
//! `req.extensions().get::<ClientView>()` (or via the `Extension(view):
//! Extension<ClientView>` extractor).

use axum::{
    extract::{Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use serde_json::json;

use crate::SharedState;

use super::{verify, PrefixedToken};

/// Per-request authorization view attached to extensions on success.
///
/// Downstream handlers read this instead of consulting the body's OpenAI
/// `user` field for authorization purposes. The body field is still
/// forwarded to the backend unchanged — it serves as a backend-side
/// usage tag, not as a nanoguard policy decision.
#[derive(Debug, Clone)]
pub struct ClientView {
    pub token_id: i64,
    pub token_prefix: String,
    pub user_id: i64,
    pub label: Option<String>,
}

/// Axum middleware fn.
///
/// Wire with `axum::middleware::from_fn_with_state(shared, verify_request)`
/// on a Router that already has `.with_state(shared)` applied.
pub async fn verify_request(
    State(shared): State<SharedState>,
    mut req: Request,
    next: Next,
) -> Response {
    let state = shared.load_full();
    let Some(auth) = state.client_auth.as_ref() else {
        // ClientAuth not opened at startup — Stage 1 with both [auth]
        // and [budget] disabled. Bypass.
        return next.run(req).await;
    };
    if !auth.enabled() {
        // Table is open (so admin endpoints can issue tokens) but
        // enforcement is off — Stage 1 default.
        return next.run(req).await;
    }

    // Optional transport check. When `[auth].require_https = true`, the
    // request must arrive with `X-Forwarded-Proto: https` (set by a
    // trusted upstream TLS terminator). The check defends against a
    // misconfiguration where bearer tokens travel plaintext.
    //
    // Loopback note: this check has no concept of "the client is on
    // localhost so skip" — that would require ConnectInfo plumbing
    // that the server isn't currently set up for. Operators running on
    // loopback should leave `require_https = false`. The config field
    // doc covers this.
    if auth.require_https()
        && req
            .headers()
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            != Some("https")
    {
        return forbidden(
            "https required",
            "this nanoguard requires X-Forwarded-Proto: https from a trusted TLS terminator",
        );
    }

    let header_val = req.headers().get(header::AUTHORIZATION);
    let Some(bearer) = extract_bearer(header_val) else {
        return unauthorized("missing bearer", "Bearer realm=\"nanoguard\"");
    };

    let Some(parsed) = PrefixedToken::parse(bearer) else {
        return unauthorized(
            "malformed token",
            "Bearer realm=\"nanoguard\", error=\"invalid_token\"",
        );
    };

    let cached = match auth.lookup(&parsed.prefix) {
        Ok(Some(c)) => c,
        Ok(None) => {
            // Same body as the hash-mismatch case below to defeat
            // prefix enumeration via response-content side-channels.
            return unauthorized(
                "unknown token",
                "Bearer realm=\"nanoguard\", error=\"invalid_token\"",
            );
        }
        Err(e) => {
            tracing::warn!(
                "client_auth: DB lookup failed for prefix {}: {}",
                parsed.prefix,
                e
            );
            return internal_error("auth lookup failed");
        }
    };
    let row = cached.row;
    let stored_hash = cached.hash;

    if !verify(parsed.wire, &stored_hash) {
        return unauthorized(
            "unknown token",
            "Bearer realm=\"nanoguard\", error=\"invalid_token\"",
        );
    }

    if row.revoked_at.is_some() {
        return unauthorized(
            "token revoked",
            "Bearer realm=\"nanoguard\", error=\"invalid_token\"",
        );
    }
    if let Some(exp) = row.expires_at.as_ref() {
        if is_past(exp) {
            return unauthorized(
                "token expired",
                "Bearer realm=\"nanoguard\", error=\"invalid_token\"",
            );
        }
    }

    let view = ClientView {
        token_id: row.id,
        token_prefix: row.prefix,
        user_id: row.user_id,
        label: row.label,
    };
    req.extensions_mut().insert(view);

    next.run(req).await
}

fn extract_bearer(h: Option<&HeaderValue>) -> Option<&str> {
    let s = h?.to_str().ok()?;
    s.strip_prefix("Bearer ")
}

fn unauthorized(detail: &'static str, www_authenticate: &'static str) -> Response {
    let mut resp = (StatusCode::UNAUTHORIZED, Json(json!({ "error": detail }))).into_response();
    if let Ok(val) = HeaderValue::from_str(www_authenticate) {
        resp.headers_mut().insert(header::WWW_AUTHENTICATE, val);
    }
    resp
}

fn forbidden(error: &'static str, hint: &'static str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "error": error, "hint": hint })),
    )
        .into_response()
}

fn internal_error(detail: &'static str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": detail })),
    )
        .into_response()
}

/// Best-effort RFC3339 "is in the past" check. Conservative on parse
/// failure: a malformed timestamp is treated as "not expired" so a
/// corrupt expiry doesn't lock a user out — but the operator gets a
/// warn log so the underlying corruption surfaces.
fn is_past(rfc3339: &str) -> bool {
    match DateTime::parse_from_rfc3339(rfc3339) {
        Ok(dt) => dt.with_timezone(&Utc) < Utc::now(),
        Err(e) => {
            tracing::warn!(
                "client_auth: malformed expires_at {:?}: {} — treating as not expired",
                rfc3339,
                e
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    //! Full-flow testing of the middleware (Authorization → DB lookup →
    //! ClientView attached) requires an axum router, which is what the
    //! e2e suite is for. The unit tests here cover the pure helpers
    //! and the type contract.

    use super::*;
    use crate::client_auth::{store, AuthConfig, ClientAuth, Token};

    #[test]
    fn extract_bearer_happy_path() {
        let h = HeaderValue::from_static("Bearer ng_p_a1b2c3d4e5f6g7h8j9k0m1n2");
        assert_eq!(
            extract_bearer(Some(&h)),
            Some("ng_p_a1b2c3d4e5f6g7h8j9k0m1n2")
        );
    }

    #[test]
    fn extract_bearer_rejects_wrong_scheme() {
        let h = HeaderValue::from_static("Basic dXNlcjpwYXNz");
        assert_eq!(extract_bearer(Some(&h)), None);
    }

    #[test]
    fn extract_bearer_rejects_missing_space() {
        let h = HeaderValue::from_static("Bearerng_p_a1b2c3d4e5f6g7h8j9k0m1n2");
        assert_eq!(extract_bearer(Some(&h)), None);
    }

    #[test]
    fn extract_bearer_handles_none() {
        assert_eq!(extract_bearer(None), None);
    }

    #[test]
    fn is_past_returns_true_for_past_timestamps() {
        assert!(is_past("2020-01-01T00:00:00Z"));
    }

    #[test]
    fn is_past_returns_false_for_future_timestamps() {
        // 100 years out; safely future for any conceivable test-runner clock.
        assert!(!is_past("2125-01-01T00:00:00Z"));
    }

    #[test]
    fn is_past_returns_false_for_malformed_strings() {
        // Conservative: a malformed expiry must not lock users out.
        assert!(!is_past("not-a-timestamp"));
        assert!(!is_past(""));
    }

    /// Compile-time check: `ClientView` is `Clone + Send + Sync` so
    /// axum can keep it in request extensions across `.await` points.
    #[test]
    fn client_view_is_send_sync_clone() {
        fn assert_send_sync_clone<T: Send + Sync + Clone>() {}
        assert_send_sync_clone::<ClientView>();
    }

    /// The DB seam the middleware uses — insert via store, look up via
    /// the same ClientAuth handle, verify the hash matches. A regression
    /// in either side fails here without needing the full axum stack.
    #[test]
    fn token_lookup_via_handle_finds_inserted_row() {
        let ca = ClientAuth::open(":memory:", AuthConfig::default()).unwrap();
        let t = Token::generate('p');
        ca.with_conn(|c| store::insert(c, &t.prefix, &t.hash, 7, Some("svc"), None).unwrap());
        let (row, hash) = ca
            .with_conn(|c| store::lookup_by_prefix(c, &t.prefix))
            .unwrap()
            .expect("present");
        assert_eq!(row.user_id, 7);
        assert!(verify(&t.wire, &hash));
    }
}
