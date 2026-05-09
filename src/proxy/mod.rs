use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{info, warn};

use crate::{matcher::InputVerdict, AppState};

/// POST /v1/chat/completions
pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    // Extract all message content for scanning
    let user_text = extract_messages_text(&body);

    // Input Guardrails
    if state.config.input.enabled {
        match state.matchers.check_input(&user_text) {
            InputVerdict::Blocked(word) => {
                warn!("BLOCK input: {:?}", word);
                return blocked_response(&format!("prompt injection detected (`{word}`)"));
            }
            InputVerdict::Alert(word) => {
                info!("ALERT input: {:?}", word);
                // Continue but log
            }
            InputVerdict::Flagged(info_str) => {
                info!("FLAG input: {}", info_str);
                // Continue but log
            }
            InputVerdict::Clean => {}
        }
    }

    // Forward to backend
    let backend_resp = match state.backend.forward_chat(body).await {
        Ok(r) => r,
        Err(e) => {
            warn!("backend error: {}", e);
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    let status = backend_resp.status();
    let is_stream = backend_resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("text/event-stream"))
        .unwrap_or(false);

    if is_stream {
        // Streaming: pass through (output filter on streaming is Phase 2)
        let headers = backend_resp.headers().clone();
        let body_stream = backend_resp.bytes_stream();
        use axum::body::Body;
        use futures_util::TryStreamExt;
        let body = Body::from_stream(body_stream.map_err(std::io::Error::other));
        let mut resp = Response::builder().status(status.as_u16());
        for (k, v) in &headers {
            resp = resp.header(k, v);
        }
        resp.body(body)
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    } else {
        // Non-streaming: apply output filter
        let resp_json: Value = match backend_resp.json().await {
            Ok(v) => v,
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({"error": e.to_string()})),
                )
                    .into_response()
            }
        };

        let filtered = if state.config.output.enabled {
            filter_response_json(&state, resp_json)
        } else {
            resp_json
        };

        (
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK),
            Json(filtered),
        )
            .into_response()
    }
}

/// GET /v1/models — proxy to backend
pub async fn list_models(State(state): State<Arc<AppState>>) -> Response {
    let url = format!("{}/v1/models", state.backend_endpoint());
    match state.http_client.get(&url).send().await {
        Ok(r) => {
            let status = r.status();
            match r.json::<Value>().await {
                Ok(v) => (
                    StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK),
                    Json(v),
                )
                    .into_response(),
                Err(_) => StatusCode::BAD_GATEWAY.into_response(),
            }
        }
        Err(_) => StatusCode::BAD_GATEWAY.into_response(),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn extract_messages_text(body: &Value) -> String {
    body.get("messages")
        .and_then(|m| m.as_array())
        .map(|msgs| {
            msgs.iter()
                .filter_map(|m| m.get("content")?.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

fn filter_response_json(state: &AppState, mut resp: Value) -> Value {
    if let Some(choices) = resp.get_mut("choices").and_then(|c| c.as_array_mut()) {
        for choice in choices.iter_mut() {
            if let Some(content) = choice
                .get_mut("message")
                .and_then(|m| m.get_mut("content"))
                .and_then(|c| c.as_str())
                .map(|s| s.to_string())
            {
                let filtered = state.matchers.filter_output(&content);
                if let Some(msg) = choice.get_mut("message") {
                    msg["content"] = Value::String(filtered);
                }
            }
        }
    }
    resp
}

fn blocked_response(reason: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": {
                "message": format!("nanoguard: request blocked — {reason}"),
                "type": "content_filter",
                "code": "blocked"
            }
        })),
    )
        .into_response()
}
