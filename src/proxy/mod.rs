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
    guard::{
        deanonymize,
        vault::{LocalVault, Vault},
    },
    matcher::InputVerdict,
    AppState, SharedState,
};

/// POST /v1/chat/completions
pub async fn chat_completions(
    State(shared): State<SharedState>,
    Json(mut body): Json<Value>,
) -> Response {
    // Snapshot the live config-derived state for the lifetime of this
    // request. A reload mid-request affects the next request, not this one.
    let state = shared.load_full();
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

    // Per-request Vault for reversible PII redaction. Built unconditionally
    // (cheap — just a few empty hashmaps) so the rest of the handler can
    // hand it through the streaming closure without conditional plumbing.
    let vault = Arc::new(std::sync::Mutex::new({
        use crate::guard::vault::PlaceholderTemplate;
        let template = match state.redactor.style() {
            redact::PlaceholderStyle::LlmGuard => PlaceholderTemplate::LlmGuard,
            _ => PlaceholderTemplate::Indexed,
        };
        LocalVault::with_style(template)
    }));

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
        // Mask-class entities are redacted in place. When reversible mode is
        // on, route through the Vault so the output deanonymizer can restore
        // originals after the LLM round-trip.
        if !state.pii_actions.mask.is_empty() {
            let masked = if state.config.input.pii.reversible {
                state.redactor.redact_messages_with_vault(
                    &mut body,
                    &state.pii_actions.mask,
                    &mut *vault.lock().unwrap(),
                )
            } else {
                state
                    .redactor
                    .redact_messages_in(&mut body, &state.pii_actions.mask)
            };
            if masked {
                info!("MASK input: PII redacted before forwarding");
            }
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

    // Spotlighting: tag untrusted (tool/function) message content so the LLM
    // treats it as data. Runs *after* PII redaction so the placeholders are
    // already in place when datamarking applies.
    if let Some(sl) = state.spotlight.as_ref() {
        if crate::guard::spotlight::apply(&mut body, sl) {
            info!("SPOTLIGHT applied to untrusted-role messages");
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
        let reversible = state.config.input.pii.reversible;
        let deanon_strategy_name = state.config.input.pii.deanonymize_strategy.clone();
        let vault_for_stream = Arc::clone(&vault);

        // Buffer SSE bytes across chunk boundaries, splitting on the blank-line
        // event terminator before applying the output filter to each event's
        // `delta.content`. This handles backends that pack multiple events into
        // one TCP chunk or split a single event across chunks.
        //
        // The stream also opportunistically extracts a `usage` field from any
        // event that carries one (OpenAI emits usage on the final chunk when
        // `stream_options.include_usage` is set). The last observed usage wins,
        // and is recorded against the budget after the stream completes.
        //
        // When reversible PII redaction is on, a DeanonymizeStream wraps the
        // output filter so placeholders that straddle SSE chunk boundaries are
        // still resolved against the per-request Vault.
        let deanon_stream: Option<
            Arc<std::sync::Mutex<crate::guard::sse_deanon::DeanonymizeStream>>,
        > = if reversible {
            let entries = vault_for_stream.lock().unwrap().entries();
            if entries.is_empty() {
                None
            } else {
                Some(Arc::new(std::sync::Mutex::new(
                    crate::guard::sse_deanon::DeanonymizeStream::new(
                        entries,
                        deanonymize::strategy_from_name(&deanon_strategy_name),
                    ),
                )))
            }
        } else {
            None
        };
        let deanon_for_closure = deanon_stream.clone();
        let mut filter = sse::SseFilter::new(move |s: &str| {
            let after_filter = matchers.filter_output(s);
            if let Some(d) = &deanon_for_closure {
                d.lock().unwrap().push(&after_filter)
            } else {
                after_filter
            }
        });
        // Streaming tool gate accumulator. None means the gate is disabled
        // for this request; otherwise events are inspected as they pass.
        let mut stream_gate = state
            .tool_gate
            .as_ref()
            .map(|g| crate::guard::sse_tool_gate::StreamingToolGate::new(Arc::clone(g)));
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
                // Streaming tool-call inspection. We re-parse each SSE event
                // payload to feed the accumulator. On a Deny outcome the
                // accumulator emits a synthetic `tool_call_denied` SSE event
                // and we stop forwarding upstream chunks.
                if let Some(gate) = stream_gate.as_mut() {
                    if let Ok(text) = std::str::from_utf8(&chunk) {
                        for raw_event in text.split("\n\n") {
                            for line in raw_event.split('\n') {
                                let line = line.strip_suffix('\r').unwrap_or(line);
                                let payload = match line.strip_prefix("data:") {
                                    Some(p) => p.trim_start(),
                                    None => continue,
                                };
                                if payload == "[DONE]" || payload.is_empty() {
                                    continue;
                                }
                                let Ok(val): Result<Value, _> = serde_json::from_str(payload)
                                else {
                                    continue;
                                };
                                if let crate::guard::sse_tool_gate::StreamGateOutcome::Deny {
                                    reasons,
                                } = gate.observe(&val)
                                {
                                    warn!(
                                        "streaming tool gate denied {} call(s); ending stream",
                                        reasons.len()
                                    );
                                    yield Ok(
                                        crate::guard::sse_tool_gate::StreamingToolGate::deny_event(&reasons),
                                    );
                                    return;
                                }
                            }
                        }
                    }
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
            // Drain any half-buffered placeholder text from the deanon stream.
            // In practice this is empty for completed responses; if non-empty
            // it indicates an unclosed `[` in the LLM output, which we log
            // for diagnostics and discard rather than leaking partial bytes
            // back into the SSE channel.
            if let Some(d) = &deanon_stream {
                let leftover = d.lock().unwrap().flush();
                if !leftover.is_empty() {
                    tracing::debug!(
                        "deanon stream had {} bytes pending at end-of-stream",
                        leftover.len()
                    );
                }
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

        let mut filtered = if state.config.output.enabled {
            filter_response_json(&state, resp_json)
        } else {
            resp_json
        };

        // Reversible deanonymize: replace placeholders in the response with
        // the original values stored in the per-request Vault.
        if state.config.input.pii.reversible {
            let entries = vault.lock().unwrap().entries();
            if !entries.is_empty() {
                let strategy =
                    deanonymize::strategy_from_name(&state.config.input.pii.deanonymize_strategy);
                deanon_response_json(&mut filtered, strategy.as_ref(), &entries);
            }
        }

        // Tool gate: inspect each tool_call, drop denied ones, replace
        // sanitized arguments. Runs before schema validation so the schema
        // sees the gated tool calls.
        if let Some(gate) = state.tool_gate.as_ref() {
            apply_tool_gate(&mut filtered, gate);
        }

        // JSON Schema validation against the assistant's response.
        if let Some(validator) = state.schema.as_ref() {
            if let Some(rule) = validator.pick("/v1/chat/completions", &model) {
                let raw = filtered
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
                        "schema violation against rule `{}`: {} error(s)",
                        rule.name,
                        outcome.errors.len()
                    );
                    match validator.on_violation() {
                        ViolationAction::Reject => {
                            return blocked_response(&format!(
                                "response failed schema `{}`: {}",
                                rule.name,
                                outcome.errors.join("; ")
                            ));
                        }
                        ViolationAction::LogOnly | ViolationAction::Repair => {
                            // Repair is not implemented yet — fall through.
                        }
                    }
                } else if let Some(extracted) = outcome.extracted {
                    // Wrapper (markdown fence / prose) was stripped — emit the
                    // cleaned JSON to the client so downstream code doesn't
                    // need to repeat the cleanup.
                    if let Some(choices) =
                        filtered.get_mut("choices").and_then(|c| c.as_array_mut())
                    {
                        if let Some(first) = choices.first_mut() {
                            if let Some(msg) = first.get_mut("message") {
                                msg["content"] = Value::String(extracted.to_string());
                            }
                        }
                    }
                }
            }
        }

        (
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK),
            Json(filtered),
        )
            .into_response()
    }
}

/// GET /v1/models — proxy to backend
pub async fn list_models(State(shared): State<SharedState>) -> Response {
    let state = shared.load_full();
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

/// Walk every choices[].message.tool_calls and run the tool gate. Denied
/// tool calls are removed; sanitized arguments are written back into the
/// `function.arguments` JSON string. If a denial happens, a synthetic
/// `nanoguard_denied_tools` entry is added to message.metadata so the
/// caller can see what was dropped.
pub(crate) fn apply_tool_gate(resp: &mut Value, gate: &crate::guard::tool_gate::ToolGate) {
    use crate::guard::tool_gate::ToolDecision;
    let Some(choices) = resp.get_mut("choices").and_then(|c| c.as_array_mut()) else {
        return;
    };
    for choice in choices {
        let Some(msg) = choice.get_mut("message") else {
            continue;
        };
        let Some(tool_calls) = msg.get_mut("tool_calls").and_then(|tc| tc.as_array_mut()) else {
            continue;
        };
        let mut denials: Vec<Value> = Vec::new();
        let mut kept: Vec<Value> = Vec::new();
        for tc in tool_calls.drain(..) {
            match gate.evaluate_openai_tool_call(&tc) {
                ToolDecision::Allow => kept.push(tc),
                ToolDecision::Sanitize { redacted_args } => {
                    let mut new_tc = tc.clone();
                    if let Some(func) = new_tc.get_mut("function") {
                        if let Some(obj) = func.as_object_mut() {
                            obj.insert(
                                "arguments".to_string(),
                                Value::String(redacted_args.to_string()),
                            );
                        }
                    }
                    info!("tool gate: sanitized arguments for tool call");
                    kept.push(new_tc);
                }
                ToolDecision::Deny { reason } => {
                    let name = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("?")
                        .to_string();
                    warn!("tool gate: denied `{name}` — {reason}");
                    denials.push(json!({"name": name, "reason": reason}));
                }
            }
        }
        *tool_calls = kept;
        if !denials.is_empty() {
            msg["nanoguard_denied_tools"] = Value::Array(denials);
        }
    }
}

/// Walk every `choices[].message.content` field (OpenAI non-streaming shape)
/// and apply a deanonymize strategy in place.
fn deanon_response_json(
    resp: &mut Value,
    strategy: &dyn deanonymize::MatchingStrategy,
    entries: &[(String, String)],
) {
    let Some(choices) = resp.get_mut("choices").and_then(|c| c.as_array_mut()) else {
        return;
    };
    for choice in choices {
        if let Some(content) = choice
            .get_mut("message")
            .and_then(|m| m.get_mut("content"))
            .and_then(|c| c.as_str())
            .map(|s| s.to_string())
        {
            let restored = strategy.restore(&content, entries);
            if let Some(msg) = choice.get_mut("message") {
                msg["content"] = Value::String(restored);
            }
        }
    }
}

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
    // Look up the matched rule in the policy index, if any. The shadow_block
    // prefix is stripped first so demoted rules still surface their metadata.
    let mut rule_id: Option<String> = None;
    let mut category: Option<String> = None;
    let mut severity: Option<String> = None;
    let mut compliance: Vec<String> = Vec::new();
    if let (Some(idx), Some(matched)) = (state.policy.as_ref(), matched_rule.as_ref()) {
        let key = matched.strip_prefix("shadow_block:").unwrap_or(matched);
        if let Some(meta) = idx.lookup_literal(key) {
            rule_id = Some(meta.id.clone());
            category = Some(meta.category.clone());
            severity = Some(meta.severity.clone());
            compliance = meta.compliance.clone();
        } else if let Some(meta) = idx.lookup_placeholder(key) {
            rule_id = Some(meta.id.clone());
            category = Some(meta.category.clone());
            severity = Some(meta.severity.clone());
            compliance = meta.compliance.clone();
        }
    }
    let entry = AuditEntry {
        request_id: request_id.to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        api_key: api_key.to_string(),
        model: model.to_string(),
        prompt_hash,
        verdict,
        matched_rule,
        rule_id,
        category,
        severity,
        compliance,
        latency_us,
    };
    log.write(&entry);
}
