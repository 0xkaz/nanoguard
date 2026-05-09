pub mod anthropic;
pub mod redact;
mod sse;

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
    audit::{AuditEntry, Verdict},
    budget::store::{BudgetCheck, SpendRecord},
    matcher::InputVerdict,
    AppState,
};

/// POST /v1/chat/completions
pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    Json(mut body): Json<Value>,
) -> Response {
    let t0 = std::time::Instant::now();
    let request_id = crate::audit::new_request_id();

    // Extract all message content for scanning
    let user_text = extract_messages_text(&body);
    let api_key = extract_api_key(&body);
    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    // Input Guardrails
    if state.config.input.enabled {
        match state
            .matchers
            .check_input_with_shadow(&user_text, state.config.input.shadow)
        {
            InputVerdict::Blocked(word) => {
                warn!("BLOCK input: {:?}", word);
                write_audit(
                    &state,
                    &request_id,
                    &api_key,
                    &model,
                    &user_text,
                    Verdict::Block,
                    Some(word.clone()),
                    t0.elapsed().as_micros() as u64,
                );
                return blocked_response(&format!("prompt injection detected (`{word}`)"));
            }
            InputVerdict::Alert(word) => {
                info!("ALERT input: {:?}", word);
            }
            InputVerdict::Flagged(info_str) => {
                info!("FLAG input: {}", info_str);
            }
            InputVerdict::Clean => {}
        }
    }

    if state.config.input.pii.enabled {
        // Reject takes precedence: any reject-class entity match short-circuits.
        if !state.pii_actions.reject.is_empty()
            && state
                .redactor
                .contains_pii_in(&user_text, &state.pii_actions.reject)
        {
            warn!("BLOCK input: PII detected (reject-class entity)");
            write_audit(
                &state,
                &request_id,
                &api_key,
                &model,
                &user_text,
                Verdict::Block,
                Some("pii".to_string()),
                t0.elapsed().as_micros() as u64,
            );
            return blocked_response("PII detected");
        }
        // Mask-class entities are redacted in place.
        if !state.pii_actions.mask.is_empty()
            && state
                .redactor
                .redact_messages_in(&mut body, &state.pii_actions.mask)
        {
            info!("MASK input: PII redacted before forwarding");
        }
        // Log-class entities are recorded but pass through unchanged.
        if !state.pii_actions.log.is_empty()
            && state
                .redactor
                .contains_pii_in(&user_text, &state.pii_actions.log)
        {
            info!("ALERT input: PII detected (log-class entity)");
        }
    }

    // Budget check (before forwarding to LLM)
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
        let budget_for_stream = state.budget.clone();
        let api_key_for_stream = api_key.clone();

        // Buffer SSE bytes across chunk boundaries, splitting on the blank-line
        // event terminator before applying the output filter to each event's
        // `delta.content`. This handles backends that pack multiple events into
        // one TCP chunk or split a single event across chunks.
        //
        // The stream also opportunistically extracts a `usage` field from any
        // event that carries one (OpenAI emits usage on the final chunk when
        // `stream_options.include_usage` is set). The last observed usage wins,
        // and is recorded against the budget after the stream completes.
        let mut filter = sse::SseFilter::new(move |s: &str| matchers.filter_output(s));
        let event_stream = body_stream.map_err(std::io::Error::other);
        let filtered = async_stream::stream! {
            futures_util::pin_mut!(event_stream);
            let mut last_usage: Option<(u64, u64, String)> = None;
            while let Some(chunk) = event_stream.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        yield Err::<bytes::Bytes, std::io::Error>(e);
                        return;
                    }
                };
                if let Some(u) = sse::SseFilter::<fn(&str) -> String>::try_extract_usage(&chunk) {
                    last_usage = Some(u);
                }
                if !output_enabled {
                    yield Ok(chunk);
                    continue;
                }
                let out = filter.push(&chunk);
                if !out.is_empty() {
                    yield Ok(out);
                }
            }
            let tail = filter.flush();
            if !tail.is_empty() {
                yield Ok(tail);
            }
            // Record streaming spend if usage was observed and a budget is wired up.
            if let (Some(budget), Some((prompt, completion, resp_model))) =
                (budget_for_stream.as_ref(), last_usage)
            {
                let record = SpendRecord {
                    api_key: api_key_for_stream,
                    prompt_tokens: prompt,
                    completion_tokens: completion,
                    model: resp_model,
                    created_at: chrono::Utc::now(),
                };
                if let Err(e) = budget.record_spend(&record).await {
                    tracing::warn!("budget record_spend (streaming) error: {}", e);
                }
            }
        };

        write_audit(
            &state,
            &request_id,
            &api_key,
            &model,
            &user_text,
            Verdict::Allow,
            None,
            t0.elapsed().as_micros() as u64,
        );

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
            let resp_model = resp_json
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let record = SpendRecord {
                api_key: api_key.clone(),
                prompt_tokens: prompt,
                completion_tokens: completion,
                model: resp_model,
                created_at: chrono::Utc::now(),
            };
            if let Err(e) = budget.record_spend(&record).await {
                tracing::warn!("budget record_spend error: {}", e);
            }
        }

        write_audit(
            &state,
            &request_id,
            &api_key,
            &model,
            &user_text,
            Verdict::Allow,
            None,
            t0.elapsed().as_micros() as u64,
        );

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

#[allow(clippy::too_many_arguments)]
fn write_audit(
    state: &AppState,
    request_id: &str,
    api_key: &str,
    model: &str,
    prompt: &str,
    verdict: Verdict,
    matched_rule: Option<String>,
    latency_us: u64,
) {
    let Some(log) = &state.audit else { return };
    let prompt_hash = log.hash_prompt(prompt);
    let entry = AuditEntry {
        request_id: request_id.to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        api_key: api_key.to_string(),
        model: model.to_string(),
        prompt_hash,
        verdict,
        matched_rule,
        latency_us,
    };
    log.write(&entry);
}

