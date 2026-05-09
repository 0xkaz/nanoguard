use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use crate::AppState;

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

// ── Handlers ──────────────────────────────────────────────────────────────────

/// GET /v1/admin/budget/:api_key — get usage and limit
pub async fn get_budget(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(api_key): Path<String>,
) -> Response {
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
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(api_key): Path<String>,
    Json(body): Json<SetLimitRequest>,
) -> Response {
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
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(api_key): Path<String>,
) -> Response {
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
