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

use crate::{
    guard::{
        deanonymize,
        vault::{LocalVault, PlaceholderTemplate, Vault},
    },
    matcher::InputVerdict,
    proxy::redact,
    AppState,
};

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
    Json(mut req): Json<AnthropicRequest>,
) -> Response {
    // Streaming is not yet supported on /v1/messages. Refuse early with a
    // 400 rather than half-handling the SSE response from the backend
    // (which would surface as an opaque 502 to the caller).
    if req.stream {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "type": "error",
                "error": {
                    "type": "invalid_request_error",
                    "message": "nanoguard: /v1/messages does not yet support stream=true; see README"
                }
            })),
        )
            .into_response();
    }

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

    // Per-request Vault for reversible redaction (cheap when unused).
    let template = match state.redactor.style() {
        redact::PlaceholderStyle::LlmGuard => PlaceholderTemplate::LlmGuard,
        _ => PlaceholderTemplate::Indexed,
    };
    let mut vault = LocalVault::with_style(template);

    // PII redaction with per-entity action overrides.
    if state.config.input.pii.enabled {
        if !state.pii_actions.reject.is_empty()
            && state
                .redactor
                .contains_pii_in(&user_text, &state.pii_actions.reject)
        {
            warn!("BLOCK input (anthropic): PII detected (reject-class entity)");
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "type": "error",
                    "error": {
                        "type": "invalid_request_error",
                        "message": "nanoguard: request blocked — PII detected"
                    }
                })),
            )
                .into_response();
        }
        if !state.pii_actions.mask.is_empty() {
            let mut redacted_any = false;
            let reversible = state.config.input.pii.reversible;
            for msg in req.messages.iter_mut() {
                match &mut msg.content {
                    AnthropicContent::Text(s) => {
                        let r = if reversible {
                            state.redactor.redact_text_with_vault(
                                s,
                                &state.pii_actions.mask,
                                &mut vault,
                            )
                        } else {
                            state.redactor.redact_text_in(s, &state.pii_actions.mask)
                        };
                        if &r != s {
                            *s = r;
                            redacted_any = true;
                        }
                    }
                    AnthropicContent::Blocks(blocks) => {
                        for b in blocks {
                            if let Some(t) = b.text.as_mut() {
                                let r = if reversible {
                                    state.redactor.redact_text_with_vault(
                                        t,
                                        &state.pii_actions.mask,
                                        &mut vault,
                                    )
                                } else {
                                    state.redactor.redact_text_in(t, &state.pii_actions.mask)
                                };
                                if &r != t {
                                    *t = r;
                                    redacted_any = true;
                                }
                            }
                        }
                    }
                }
            }
            if redacted_any {
                info!("MASK input (anthropic): PII redacted before forwarding");
            }
        }
        if !state.pii_actions.log.is_empty()
            && state
                .redactor
                .contains_pii_in(&user_text, &state.pii_actions.log)
        {
            info!("ALERT input (anthropic): PII detected (log-class entity)");
        }
    }

    // Convert Anthropic → OpenAI format (after redaction so the LLM sees the
    // redacted prompt).
    let mut oai_messages: Vec<Value> = vec![];
    if let Some(system) = &req.system {
        oai_messages.push(json!({"role": "system", "content": system}));
    }
    for msg in &req.messages {
        oai_messages.push(json!({"role": msg.role, "content": msg.content.as_text()}));
    }

    let mut oai_body = json!({
        "model": req.model,
        "messages": oai_messages,
        "stream": req.stream,
        "max_tokens": req.max_tokens.unwrap_or(1024),
    });

    // Spotlighting: tag untrusted-role messages so the LLM treats them as
    // data. Same module used by /v1/chat/completions; runs after the
    // Anthropic→OpenAI normalization so the input pipeline behavior is
    // consistent across endpoints.
    if let Some(sl) = state.spotlight.as_ref() {
        if crate::guard::spotlight::apply(&mut oai_body, sl) {
            info!("SPOTLIGHT applied (anthropic) to untrusted-role messages");
        }
    }

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
    let mut oai_resp: Value =
        match backend_resp.json().await {
            Ok(v) => v,
            Err(e) => return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"type":"error","error":{"type":"api_error","message":e.to_string()}})),
            )
                .into_response(),
        };

    // Tool gate runs against the OpenAI-shaped response. Denied tool calls
    // are removed from `tool_calls` before we re-shape into Anthropic format.
    if let Some(gate) = state.tool_gate.as_ref() {
        crate::proxy::apply_tool_gate(&mut oai_resp, gate);
    }

    // Schema validation against the assistant's response (string content path).
    if let Some(validator) = state.schema.as_ref() {
        if let Some(rule) = validator.pick("/v1/messages", &req.model) {
            let raw = oai_resp
                .get("choices")
                .and_then(|c| c.as_array())
                .and_then(|cs| cs.first())
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("content"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let outcome =
                crate::guard::schema::SchemaValidator::validate_response_text(&rule, raw);
            if !outcome.passed {
                use crate::guard::schema::ViolationAction;
                warn!(
                    "schema violation (anthropic) against rule `{}`: {} error(s)",
                    rule.name,
                    outcome.errors.len()
                );
                if matches!(validator.on_violation(), ViolationAction::Reject) {
                    return (
                        StatusCode::BAD_GATEWAY,
                        Json(json!({
                            "type": "error",
                            "error": {
                                "type": "api_error",
                                "message": format!("response failed schema `{}`", rule.name)
                            }
                        })),
                    )
                        .into_response();
                }
            }
        }
    }

    // Extract textual content + any surviving tool_calls from the OpenAI
    // response (after tool gate / schema). Build the Anthropic content[] from
    // both — text becomes a `{"type":"text"}` block, each tool_call becomes
    // a `{"type":"tool_use"}` block.
    let oai_message = oai_resp
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .cloned()
        .unwrap_or(Value::Null);

    let content_text = oai_message
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();

    // Output filter
    let content_text = if state.config.output.enabled {
        state.matchers.filter_output(&content_text)
    } else {
        content_text
    };

    // Reversible deanonymize from Vault entries.
    let content_text = if state.config.input.pii.reversible && !vault.is_empty() {
        let strategy =
            deanonymize::strategy_from_name(&state.config.input.pii.deanonymize_strategy);
        deanonymize::restore(strategy.as_ref(), &vault, &content_text)
    } else {
        content_text
    };

    let mut anthropic_content: Vec<Value> = Vec::new();
    if !content_text.is_empty() {
        anthropic_content.push(json!({"type": "text", "text": content_text}));
    }
    if let Some(tool_calls) = oai_message.get("tool_calls").and_then(|tc| tc.as_array()) {
        for tc in tool_calls {
            let id = tc.get("id").cloned().unwrap_or(Value::Null);
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .cloned()
                .unwrap_or(Value::String("unknown".into()));
            let input_value = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or(Value::Object(Default::default()));
            anthropic_content.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input_value,
            }));
        }
    }
    // Surface denied tools at the message level so callers can react.
    if let Some(denied) = oai_message.get("nanoguard_denied_tools") {
        anthropic_content.push(json!({
            "type": "nanoguard_denied_tools",
            "details": denied,
        }));
    }
    if anthropic_content.is_empty() {
        // Always emit at least one block.
        anthropic_content.push(json!({"type": "text", "text": ""}));
    }

    let stop_reason = if oai_message.get("tool_calls").is_some() {
        "tool_use"
    } else {
        "end_turn"
    };

    let anthropic_resp = json!({
        "id": oai_resp.get("id").and_then(|v| v.as_str()).unwrap_or("msg_nanoguard"),
        "type": "message",
        "role": "assistant",
        "model": req.model,
        "content": anthropic_content,
        "stop_reason": stop_reason,
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
