/// POST /v1/messages — Anthropic Messages API compatible endpoint
///
/// Converts Anthropic request format to OpenAI format, proxies through guardrails,
/// then converts the response back to Anthropic format.
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{info, warn};

use crate::{matcher::InputVerdict, AppState};

#[derive(Debug, Deserialize)]
pub struct AnthropicRequest {
    pub model: String,
    pub messages: Vec<AnthropicMessage>,
    pub max_tokens: Option<u32>,
    pub system: Option<String>,
    #[serde(default)]
    pub stream: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: AnthropicContent,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(untagged)]
pub enum AnthropicContent {
    Text(String),
    Blocks(Vec<AnthropicBlock>),
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AnthropicBlock {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: Option<String>,
}

impl AnthropicContent {
    pub fn as_text(&self) -> String {
        match self {
            AnthropicContent::Text(s) => s.clone(),
            AnthropicContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| b.text.as_deref())
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

pub async fn messages(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AnthropicRequest>,
) -> Response {
    // Collect all message text for scanning
    let user_text = req
        .messages
        .iter()
        .map(|m| m.content.as_text())
        .collect::<Vec<_>>()
        .join(" ");

    // Input Guardrails
    if state.config.input.enabled {
        match state
            .matchers
            .check_input_with_shadow(&user_text, state.config.input.shadow)
        {
            InputVerdict::Blocked(word) => {
                warn!("BLOCK input (anthropic): {:?}", word);
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "type": "error",
                        "error": {
                            "type": "invalid_request_error",
                            "message": format!("nanoguard: request blocked — prompt injection detected (`{word}`)")
                        }
                    })),
                )
                    .into_response();
            }
            InputVerdict::Alert(word) => info!("ALERT input (anthropic): {:?}", word),
            InputVerdict::Flagged(s) => info!("FLAG input (anthropic): {}", s),
            InputVerdict::Clean => {}
        }
    }

    // Convert Anthropic → OpenAI format
    let mut oai_messages: Vec<Value> = vec![];
    if let Some(system) = &req.system {
        oai_messages.push(json!({"role": "system", "content": system}));
    }
    for msg in &req.messages {
        oai_messages.push(json!({"role": msg.role, "content": msg.content.as_text()}));
    }

    let oai_body = json!({
        "model": req.model,
        "messages": oai_messages,
        "stream": req.stream,
        "max_tokens": req.max_tokens.unwrap_or(1024),
    });

    // Forward to backend (OpenAI-compatible)
    let backend_resp = match state.backend.forward_chat(oai_body).await {
        Ok(r) => r,
        Err(e) => {
            warn!("backend error (anthropic): {}", e);
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"type":"error","error":{"type":"api_error","message":e.to_string()}})),
            )
                .into_response();
        }
    };

    let status = backend_resp.status();
    let oai_resp: Value =
        match backend_resp.json().await {
            Ok(v) => v,
            Err(e) => return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"type":"error","error":{"type":"api_error","message":e.to_string()}})),
            )
                .into_response(),
        };

    // Extract content from OpenAI response
    let content_text = oai_resp
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();

    // Output filter
    let content_text = if state.config.output.enabled {
        state.matchers.filter_output(&content_text)
    } else {
        content_text
    };

    // Convert OpenAI → Anthropic response format
    let anthropic_resp = json!({
        "id": oai_resp.get("id").and_then(|v| v.as_str()).unwrap_or("msg_nanoguard"),
        "type": "message",
        "role": "assistant",
        "model": req.model,
        "content": [{"type": "text", "text": content_text}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": oai_resp.get("usage").and_then(|u| u.get("prompt_tokens")).cloned().unwrap_or(json!(0)),
            "output_tokens": oai_resp.get("usage").and_then(|u| u.get("completion_tokens")).cloned().unwrap_or(json!(0)),
        }
    });

    (
        StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK),
        Json(anthropic_resp),
    )
        .into_response()
}
