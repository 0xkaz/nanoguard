pub mod anthropic;

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{info, warn};

use crate::{
    budget::store::{BudgetCheck, SpendRecord},
    matcher::InputVerdict,
    AppState,
};

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

    // Budget check (before forwarding to LLM)
    let api_key = extract_api_key(&body);
    if let Some(budget) = &state.budget {
        match budget.check(&api_key).await {
            Ok(BudgetCheck::Exceeded { usage, limit }) => {
                warn!("budget exceeded for key={api_key}: {usage}/{limit} tokens");
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({
                        "error": {
                            "message": format!("nanoguard: token budget exceeded ({usage}/{limit})"),
                            "type": "budget_exceeded",
                            "code": "budget_exceeded"
                        }
                    })),
                )
                    .into_response();
            }
            Ok(_) => {}
            Err(e) => warn!("budget check error: {}", e),
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
        let headers = backend_resp.headers().clone();
        let body_stream = backend_resp.bytes_stream();
        use axum::body::Body;
        use futures_util::{StreamExt, TryStreamExt};

        let output_enabled = state.config.output.enabled;
        let matchers = Arc::clone(&state.matchers);

        // Apply output filter to each SSE chunk's content field.
        // Each chunk is a `data: {...}\n\n` line. We parse the JSON delta and
        // filter the `choices[].delta.content` string in-place.
        let filtered = body_stream
            .map_err(std::io::Error::other)
            .map(move |chunk| {
                let chunk = chunk?;
                if !output_enabled {
                    return Ok::<_, std::io::Error>(chunk);
                }
                // Fast path: skip non-data lines and [DONE]
                let text = match std::str::from_utf8(&chunk) {
                    Ok(t) => t,
                    Err(_) => return Ok(chunk),
                };
                if !text.starts_with("data:") || text.contains("[DONE]") {
                    return Ok(chunk);
                }
                let json_str = text.trim_start_matches("data:").trim();
                let mut val: Value = match serde_json::from_str(json_str) {
                    Ok(v) => v,
                    Err(_) => return Ok(chunk),
                };
                // Filter delta.content in each choice
                if let Some(choices) = val.get_mut("choices").and_then(|c| c.as_array_mut()) {
                    for choice in choices.iter_mut() {
                        if let Some(content) = choice
                            .get_mut("delta")
                            .and_then(|d| d.get_mut("content"))
                            .and_then(|c| c.as_str())
                            .map(|s| s.to_string())
                        {
                            let filtered = matchers.filter_output(&content);
                            if let Some(delta) = choice.get_mut("delta") {
                                delta["content"] = Value::String(filtered);
                            }
                        }
                    }
                }
                let out = format!("data: {}\n\n", val);
                Ok(bytes::Bytes::from(out))
            });

        let body = Body::from_stream(filtered);
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

        // Record spend after successful response (LiteLLM pattern: count on response)
        if let Some(budget) = &state.budget {
            let prompt = resp_json
                .get("usage")
                .and_then(|u| u.get("prompt_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let completion = resp_json
                .get("usage")
                .and_then(|u| u.get("completion_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let model = resp_json
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let record = SpendRecord {
                api_key: api_key.clone(),
                prompt_tokens: prompt,
                completion_tokens: completion,
                model,
                created_at: chrono::Utc::now(),
            };
            let budget = budget.clone();
            tokio::spawn(async move {
                if let Err(e) = budget.record_spend(&record).await {
                    tracing::warn!("budget record_spend error: {}", e);
                }
            });
        }

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

fn extract_api_key(body: &Value) -> String {
    // Use "user" field as api_key identifier if present, else "default"
    body.get("user")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string()
}

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
