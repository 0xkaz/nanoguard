//! Streaming tool-call accumulator + gate.
//!
//! OpenAI streams tool calls as a sequence of partial deltas keyed by
//! `tool_calls[].index`. The function name arrives in one delta, then the
//! `arguments` JSON arrives in chunks across many deltas, then a final
//! `finish_reason = "tool_calls"` event tells us the call is complete.
//!
//! `StreamingToolGate` reassembles those deltas, hands the completed
//! tool call off to the existing `ToolGate`, and surfaces either an
//! `Allow` (let the chunks flow on) or a `Deny` (stop the stream and
//! emit a synthetic SSE error event so the client doesn't execute a
//! denied tool).
//!
//! `Sanitize` is reported as `Allow` here: by the time we see the
//! complete arguments, the delta chunks carrying those arguments have
//! already been forwarded to the client. Streaming Sanitize would need
//! the proxy to buffer the entire tool-call delta sequence before
//! forwarding any of it — a noticeable latency cost. The non-streaming
//! path remains the place where Sanitize takes effect.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::guard::tool_gate::{ToolDecision, ToolGate};

#[derive(Default)]
struct PartialToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl PartialToolCall {
    fn merge_delta(&mut self, delta_tc: &Value) {
        if let Some(id) = delta_tc.get("id").and_then(|v| v.as_str()) {
            self.id = Some(id.to_string());
        }
        if let Some(name) = delta_tc
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
        {
            self.name = Some(name.to_string());
        }
        if let Some(args) = delta_tc
            .get("function")
            .and_then(|f| f.get("arguments"))
            .and_then(|a| a.as_str())
        {
            self.arguments.push_str(args);
        }
    }

    fn into_openai_value(self) -> Option<Value> {
        let name = self.name?;
        Some(json!({
            "id": self.id.unwrap_or_default(),
            "type": "function",
            "function": {
                "name": name,
                "arguments": if self.arguments.is_empty() {
                    "{}".to_string()
                } else {
                    self.arguments
                }
            }
        }))
    }
}

pub struct StreamingToolGate {
    gate: Arc<ToolGate>,
    /// Per-index partial accumulators for the (possibly several) tool calls
    /// the model may emit in a single response.
    partials: HashMap<u64, PartialToolCall>,
    /// True once we have flagged at least one tool call as Deny. Once true
    /// the proxy should stop forwarding further chunks and emit the
    /// synthetic error event.
    denied: Vec<String>,
}

#[derive(Debug)]
pub enum StreamGateOutcome {
    /// Nothing to act on yet. Forward the chunk as-is.
    Allow,
    /// The completed tool call set was denied. The proxy should stop
    /// forwarding upstream chunks and instead emit `error_event` once.
    Deny { reasons: Vec<String> },
}

impl StreamingToolGate {
    pub fn new(gate: Arc<ToolGate>) -> Self {
        Self {
            gate,
            partials: HashMap::new(),
            denied: Vec::new(),
        }
    }

    /// Inspect a single SSE event payload (the parsed JSON of one
    /// `data: {...}` line) and update internal state. Returns the
    /// streaming-gate outcome to apply to the *current* chunk.
    pub fn observe(&mut self, event: &Value) -> StreamGateOutcome {
        // Walk choices[].delta.tool_calls to accumulate partial deltas.
        if let Some(choices) = event.get("choices").and_then(|c| c.as_array()) {
            for choice in choices {
                if let Some(delta_calls) = choice
                    .get("delta")
                    .and_then(|d| d.get("tool_calls"))
                    .and_then(|tc| tc.as_array())
                {
                    for delta_tc in delta_calls {
                        let idx = delta_tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                        let entry = self.partials.entry(idx).or_default();
                        entry.merge_delta(delta_tc);
                    }
                }
                // finish_reason == "tool_calls" → the model is done emitting
                // tool calls; evaluate everything we accumulated.
                if let Some(finish) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                    if finish == "tool_calls" {
                        return self.evaluate_collected();
                    }
                }
            }
        }
        StreamGateOutcome::Allow
    }

    fn evaluate_collected(&mut self) -> StreamGateOutcome {
        let mut indices: Vec<u64> = self.partials.keys().copied().collect();
        indices.sort();
        let mut reasons: Vec<String> = Vec::new();
        for idx in indices {
            let partial = self.partials.remove(&idx).unwrap_or_default();
            let Some(tc_value) = partial.into_openai_value() else {
                continue;
            };
            match self.gate.evaluate_openai_tool_call(&tc_value) {
                ToolDecision::Allow | ToolDecision::Sanitize { .. } => {
                    // Sanitize is degraded to Allow in the streaming path;
                    // see module docs.
                }
                ToolDecision::Deny { reason } => {
                    reasons.push(reason);
                }
            }
        }
        if reasons.is_empty() {
            StreamGateOutcome::Allow
        } else {
            self.denied.extend(reasons.iter().cloned());
            StreamGateOutcome::Deny { reasons }
        }
    }

    /// Build a synthetic SSE error event the proxy can emit to terminate
    /// the stream when a Deny outcome was reached.
    pub fn deny_event(reasons: &[String]) -> bytes::Bytes {
        let payload = json!({
            "error": {
                "type": "tool_call_denied",
                "message": "nanoguard tool gate denied one or more tool calls",
                "reasons": reasons,
            }
        });
        let line = format!("data: {}\n\ndata: [DONE]\n\n", payload);
        bytes::Bytes::from(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::tool_gate::ToolGate;
    use crate::proxy::redact::{default_inline_patterns, PlaceholderStyle, Redactor};
    use std::collections::{HashMap as Map, HashSet};

    fn build_gate(deny: Vec<&str>) -> Arc<ToolGate> {
        let redactor = Arc::new(
            Redactor::build_with_style(&default_inline_patterns(), &[], PlaceholderStyle::Bare)
                .unwrap(),
        );
        Arc::new(ToolGate::new(
            None,
            deny.into_iter().map(String::from).collect(),
            Map::new(),
            redactor,
            HashSet::new(),
            HashSet::new(),
        ))
    }

    #[test]
    fn accumulates_and_denies_when_finish_arrives() {
        let mut s = StreamingToolGate::new(build_gate(vec!["delete_*"]));
        // delta 1: name only
        let e1 = json!({
            "choices": [{
                "delta": {"tool_calls": [{"index":0,"id":"c1","type":"function","function":{"name":"delete_record"}}]}
            }]
        });
        assert!(matches!(s.observe(&e1), StreamGateOutcome::Allow));

        // delta 2: arguments first half
        let e2 = json!({
            "choices": [{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"id"}}]}}]
        });
        assert!(matches!(s.observe(&e2), StreamGateOutcome::Allow));

        // delta 3: arguments second half + finish
        let e3 = json!({
            "choices": [{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\":1}"}}]}}]
        });
        assert!(matches!(s.observe(&e3), StreamGateOutcome::Allow));

        let e4 = json!({"choices":[{"finish_reason":"tool_calls"}]});
        match s.observe(&e4) {
            StreamGateOutcome::Deny { reasons } => {
                assert_eq!(reasons.len(), 1);
                assert!(reasons[0].contains("deny"));
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn allows_unknown_tool_when_no_rules_match() {
        let mut s = StreamingToolGate::new(build_gate(vec![]));
        let e1 = json!({
            "choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"search_kb"}}]}}]
        });
        s.observe(&e1);
        let finish = json!({"choices":[{"finish_reason":"tool_calls"}]});
        assert!(matches!(s.observe(&finish), StreamGateOutcome::Allow));
    }

    #[test]
    fn handles_multiple_tool_calls_in_one_response() {
        let mut s = StreamingToolGate::new(build_gate(vec!["delete_*"]));
        // index 0: search_kb (allowed)
        s.observe(&json!({
            "choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"search_kb","arguments":"{}"}}]}}]
        }));
        // index 1: delete_record (denied)
        s.observe(&json!({
            "choices":[{"delta":{"tool_calls":[{"index":1,"function":{"name":"delete_record","arguments":"{}"}}]}}]
        }));
        let finish = json!({"choices":[{"finish_reason":"tool_calls"}]});
        match s.observe(&finish) {
            StreamGateOutcome::Deny { reasons } => assert_eq!(reasons.len(), 1),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn deny_event_contains_done_terminator() {
        let bytes = StreamingToolGate::deny_event(&["x".to_string()]);
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.contains("nanoguard tool gate denied"));
        assert!(s.contains("[DONE]"));
    }

    #[test]
    fn ignores_events_without_tool_calls() {
        let mut s = StreamingToolGate::new(build_gate(vec!["delete_*"]));
        // a content delta — no tool calls at all
        let e = json!({"choices":[{"delta":{"content":"hello"}}]});
        assert!(matches!(s.observe(&e), StreamGateOutcome::Allow));
    }
}
