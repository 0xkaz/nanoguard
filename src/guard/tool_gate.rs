//! Tool gate: inspect each LLM-emitted tool / function call before the
//! application executes it.
//!
//! Three controls are layered:
//! 1. Allow / deny the tool by name (with `*` wildcards).
//! 2. Validate the arguments against an optional JSON Schema.
//! 3. Scan the arguments through the existing PII/secret Redactor and
//!    refuse calls whose arguments leak high-severity entities.
//!
//! The gate is stateless and does not call the network. It returns a
//! `ToolDecision` which the proxy layer consumes to either pass the tool
//! call through, sanitize its arguments, or rewrite the response so the
//! client sees an error instead of the dangerous call.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::Value;

use crate::proxy::redact::Redactor;

/// One pattern entry with simple `*` wildcard support.
#[derive(Debug, Clone)]
pub struct Pattern {
    pub raw: String,
}

impl Pattern {
    pub fn new(raw: impl Into<String>) -> Self {
        Self { raw: raw.into() }
    }

    pub fn matches(&self, name: &str) -> bool {
        let p = self.raw.as_str();
        if let Some(prefix) = p.strip_suffix('*') {
            name.starts_with(prefix)
        } else if let Some(suffix) = p.strip_prefix('*') {
            name.ends_with(suffix)
        } else {
            p == name
        }
    }
}

#[derive(Debug)]
pub enum ToolDecision {
    Allow,
    Deny { reason: String },
    Sanitize { redacted_args: Value },
}

pub struct ToolGate {
    /// `None` = open mode (anything not on `deny` is allowed).
    /// `Some(_)` = closed mode (only items matching `allow` pass).
    allow: Option<Vec<Pattern>>,
    deny: Vec<Pattern>,
    /// JSON Schema validators keyed by tool name. Compiled at startup.
    schemas: HashMap<String, jsonschema::Validator>,
    /// Redactor used to scan tool arguments for PII / secrets. Shared with
    /// the request-side redactor — same entity definitions, same dictionaries.
    redactor: Arc<Redactor>,
    /// Entities whose presence in tool arguments causes a Deny.
    reject_entities: HashSet<String>,
    /// Entities to mask in tool arguments (Sanitize). Disjoint from
    /// `reject_entities`.
    mask_entities: HashSet<String>,
}

impl ToolGate {
    pub fn new(
        allow: Option<Vec<String>>,
        deny: Vec<String>,
        schemas: HashMap<String, jsonschema::Validator>,
        redactor: Arc<Redactor>,
        reject_entities: HashSet<String>,
        mask_entities: HashSet<String>,
    ) -> Self {
        Self {
            allow: allow.map(|v| v.into_iter().map(Pattern::new).collect()),
            deny: deny.into_iter().map(Pattern::new).collect(),
            schemas,
            redactor,
            reject_entities,
            mask_entities,
        }
    }

    /// Inspect an OpenAI-shape tool call object:
    /// ```json
    /// {"id":"...","type":"function","function":{"name":"x","arguments":"{...}"}}
    /// ```
    pub fn evaluate_openai_tool_call(&self, tc: &Value) -> ToolDecision {
        let func = match tc.get("function") {
            Some(f) => f,
            None => return ToolDecision::Deny {
                reason: "tool call missing function field".into(),
            },
        };
        let name = match func.get("name").and_then(|n| n.as_str()) {
            Some(n) => n,
            None => return ToolDecision::Deny {
                reason: "tool call missing function.name".into(),
            },
        };
        // Arguments come as a JSON-encoded string (OpenAI quirk).
        let args_raw = func
            .get("arguments")
            .and_then(|a| a.as_str())
            .unwrap_or("{}");
        self.evaluate(name, args_raw)
    }

    /// Inspect an Anthropic-shape tool_use block:
    /// ```json
    /// {"type":"tool_use","id":"...","name":"x","input":{...}}
    /// ```
    pub fn evaluate_anthropic_tool_use(&self, tu: &Value) -> ToolDecision {
        let name = match tu.get("name").and_then(|n| n.as_str()) {
            Some(n) => n,
            None => return ToolDecision::Deny {
                reason: "tool_use missing name".into(),
            },
        };
        // Input is already a parsed object on the wire.
        let args_value = tu.get("input").cloned().unwrap_or(Value::Object(Default::default()));
        let args_raw = args_value.to_string();
        self.evaluate(name, &args_raw)
    }

    fn evaluate(&self, name: &str, args_json_str: &str) -> ToolDecision {
        // Step 1: name check.
        if self.deny.iter().any(|p| p.matches(name)) {
            return ToolDecision::Deny {
                reason: format!("tool `{name}` is on the deny list"),
            };
        }
        if let Some(allow) = &self.allow {
            if !allow.iter().any(|p| p.matches(name)) {
                return ToolDecision::Deny {
                    reason: format!("tool `{name}` is not on the allow list"),
                };
            }
        }

        // Step 2: JSON parse + schema check.
        let args: Value = match serde_json::from_str(args_json_str) {
            Ok(v) => v,
            Err(e) => {
                return ToolDecision::Deny {
                    reason: format!("tool `{name}` arguments are not valid JSON: {e}"),
                };
            }
        };
        if let Some(schema) = self.schemas.get(name) {
            let errors: Vec<String> = schema
                .iter_errors(&args)
                .map(|e| format!("{}: {}", e.instance_path, e))
                .collect();
            if !errors.is_empty() {
                return ToolDecision::Deny {
                    reason: format!(
                        "tool `{name}` arguments failed schema: {}",
                        errors.join("; ")
                    ),
                };
            }
        }

        // Step 3: PII / secret scan on the argument string.
        if !self.reject_entities.is_empty()
            && self
                .redactor
                .contains_pii_in(args_json_str, &self.reject_entities)
        {
            return ToolDecision::Deny {
                reason: format!(
                    "tool `{name}` arguments contain rejected entity",
                ),
            };
        }
        if !self.mask_entities.is_empty()
            && self
                .redactor
                .contains_pii_in(args_json_str, &self.mask_entities)
        {
            // Mask in the parsed JSON form by walking string leaves.
            let mut masked = args.clone();
            mask_strings_in_value(&mut masked, &self.redactor, &self.mask_entities);
            return ToolDecision::Sanitize {
                redacted_args: masked,
            };
        }

        ToolDecision::Allow
    }
}

fn mask_strings_in_value(
    value: &mut Value,
    redactor: &Redactor,
    entities: &HashSet<String>,
) {
    match value {
        Value::String(s) => {
            let masked = redactor.redact_text_in(s, entities);
            *s = masked;
        }
        Value::Array(items) => {
            for v in items {
                mask_strings_in_value(v, redactor, entities);
            }
        }
        Value::Object(map) => {
            for (_, v) in map.iter_mut() {
                mask_strings_in_value(v, redactor, entities);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::redact::{default_inline_patterns, PlaceholderStyle, Redactor};
    use serde_json::json;

    fn build_gate(
        allow: Option<Vec<&str>>,
        deny: Vec<&str>,
        reject: &[&str],
    ) -> ToolGate {
        let redactor = Arc::new(
            Redactor::build_with_style(
                &default_inline_patterns(),
                &[],
                PlaceholderStyle::Bare,
            )
            .unwrap(),
        );
        let reject_set: HashSet<String> = reject.iter().map(|s| s.to_string()).collect();
        ToolGate::new(
            allow.map(|v| v.into_iter().map(String::from).collect()),
            deny.into_iter().map(String::from).collect(),
            HashMap::new(),
            redactor,
            reject_set,
            HashSet::new(),
        )
    }

    fn openai_call(name: &str, args: Value) -> Value {
        json!({
            "id": "call_1",
            "type": "function",
            "function": {"name": name, "arguments": args.to_string()}
        })
    }

    #[test]
    fn pattern_matches_prefix() {
        assert!(Pattern::new("delete_*").matches("delete_user"));
        assert!(!Pattern::new("delete_*").matches("read_user"));
    }

    #[test]
    fn pattern_matches_suffix() {
        assert!(Pattern::new("*_admin").matches("user_admin"));
        assert!(!Pattern::new("*_admin").matches("admin_user"));
    }

    #[test]
    fn pattern_matches_exact() {
        assert!(Pattern::new("send_email").matches("send_email"));
        assert!(!Pattern::new("send_email").matches("send"));
    }

    #[test]
    fn deny_list_blocks_matching_tool() {
        let gate = build_gate(None, vec!["delete_*"], &[]);
        let tc = openai_call("delete_user", json!({"id": 1}));
        assert!(matches!(
            gate.evaluate_openai_tool_call(&tc),
            ToolDecision::Deny { .. }
        ));
    }

    #[test]
    fn allow_list_blocks_unlisted_tool() {
        let gate = build_gate(Some(vec!["search_*"]), vec![], &[]);
        let tc = openai_call("send_email", json!({"to": "x@y"}));
        assert!(matches!(
            gate.evaluate_openai_tool_call(&tc),
            ToolDecision::Deny { .. }
        ));
    }

    #[test]
    fn allow_list_passes_listed_tool() {
        let gate = build_gate(Some(vec!["search_*"]), vec![], &[]);
        let tc = openai_call("search_kb", json!({"query": "rust"}));
        assert!(matches!(
            gate.evaluate_openai_tool_call(&tc),
            ToolDecision::Allow
        ));
    }

    #[test]
    fn deny_takes_precedence_over_allow() {
        let gate = build_gate(Some(vec!["*"]), vec!["shell_exec"], &[]);
        let tc = openai_call("shell_exec", json!({"cmd": "ls"}));
        assert!(matches!(
            gate.evaluate_openai_tool_call(&tc),
            ToolDecision::Deny { .. }
        ));
    }

    #[test]
    fn malformed_arguments_are_denied() {
        let gate = build_gate(None, vec![], &[]);
        let tc = json!({
            "id": "x",
            "type": "function",
            "function": {"name": "f", "arguments": "not json"}
        });
        assert!(matches!(
            gate.evaluate_openai_tool_call(&tc),
            ToolDecision::Deny { .. }
        ));
    }

    #[test]
    fn reject_entity_in_argument_blocks_call() {
        let gate = build_gate(None, vec![], &["AWS_ACCESS_KEY_ID"]);
        let tc = openai_call(
            "send_email",
            json!({"to": "x@y.com", "body": "key is AKIAIOSFODNN7EXAMPLE"}),
        );
        match gate.evaluate_openai_tool_call(&tc) {
            ToolDecision::Deny { reason } => {
                assert!(reason.contains("rejected entity"), "got `{reason}`");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn schema_violation_blocks_call() {
        let mut schemas = HashMap::new();
        let schema = serde_json::json!({
            "type": "object",
            "required": ["to"],
            "properties": {"to": {"type": "string"}}
        });
        schemas.insert(
            "send_email".to_string(),
            jsonschema::draft202012::new(&schema).unwrap(),
        );
        let redactor = Arc::new(
            Redactor::build_with_style(
                &default_inline_patterns(),
                &[],
                PlaceholderStyle::Bare,
            )
            .unwrap(),
        );
        let gate = ToolGate::new(
            None,
            vec![],
            schemas,
            redactor,
            HashSet::new(),
            HashSet::new(),
        );
        let tc = openai_call("send_email", json!({"body": "no recipient"}));
        match gate.evaluate_openai_tool_call(&tc) {
            ToolDecision::Deny { reason } => {
                assert!(reason.contains("failed schema"), "got `{reason}`");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn anthropic_tool_use_evaluated_via_input_field() {
        let gate = build_gate(None, vec!["delete_*"], &[]);
        let tu = json!({
            "type": "tool_use",
            "id": "toolu_1",
            "name": "delete_record",
            "input": {"id": 5}
        });
        assert!(matches!(
            gate.evaluate_anthropic_tool_use(&tu),
            ToolDecision::Deny { .. }
        ));
    }

    #[test]
    fn mask_entity_returns_sanitize_with_redacted_args() {
        // Build a gate where EMAIL is in mask set.
        let redactor = Arc::new(
            Redactor::build_with_style(
                &default_inline_patterns(),
                &[],
                PlaceholderStyle::Bare,
            )
            .unwrap(),
        );
        let mut mask = HashSet::new();
        mask.insert("EMAIL".to_string());
        let gate = ToolGate::new(
            None,
            vec![],
            HashMap::new(),
            redactor,
            HashSet::new(),
            mask,
        );
        let tc = openai_call(
            "log_activity",
            json!({"user": "alice@example.com", "action": "login"}),
        );
        match gate.evaluate_openai_tool_call(&tc) {
            ToolDecision::Sanitize { redacted_args } => {
                let s = redacted_args.to_string();
                assert!(s.contains("[EMAIL]"), "got `{s}`");
                assert!(!s.contains("alice@example.com"));
            }
            other => panic!("expected Sanitize, got {other:?}"),
        }
    }
}
