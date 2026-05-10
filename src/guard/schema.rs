//! JSON Schema validator for LLM responses.
//!
//! Validates the response (or a tool-call argument JSON) against a per-route
//! / per-model schema picked at request time. On violation the proxy can
//! reject, log, or — in a future iteration — repair the payload.
//!
//! This module is deliberately stateless: a `SchemaValidator` holds compiled
//! schemas and a few rules; it doesn't know about HTTP, streaming, or the
//! request lifecycle. The proxy layer is responsible for picking the rule
//! and acting on the outcome.

use std::sync::Arc;

use anyhow::{Context, Result};
use jsonschema::Validator;
use regex::Regex;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViolationAction {
    /// Return a 502 to the client; let the caller retry / give up.
    Reject,
    /// Pass the response through unchanged but record an audit entry.
    LogOnly,
    /// Reserved for a future repair pass (json_repair port). Currently
    /// behaves like LogOnly with a warning.
    Repair,
}

impl ViolationAction {
    pub fn from_str(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "reject" => Self::Reject,
            "repair" => Self::Repair,
            _ => Self::LogOnly,
        }
    }
}

pub struct SchemaRule {
    /// Endpoint path this rule applies to (e.g. "/v1/chat/completions").
    pub endpoint: String,
    /// Regex matched against the request `model` field. `None` means "any model".
    pub model_pattern: Option<Regex>,
    pub schema: Validator,
    /// Human-readable name for logs and audit entries.
    pub name: String,
}

pub struct SchemaValidator {
    rules: Vec<Arc<SchemaRule>>,
    on_violation: ViolationAction,
}

#[derive(Debug)]
pub struct ValidationOutcome {
    pub passed: bool,
    pub errors: Vec<String>,
    /// Set when the validator extracted JSON from a wrapper (e.g. a markdown
    /// ```json fenced block) so the proxy can re-emit the cleaned payload.
    pub extracted: Option<Value>,
}

impl SchemaValidator {
    pub fn new(rules: Vec<SchemaRule>, on_violation: ViolationAction) -> Self {
        Self {
            rules: rules.into_iter().map(Arc::new).collect(),
            on_violation,
        }
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    pub fn on_violation(&self) -> ViolationAction {
        self.on_violation
    }

    /// Find the first rule whose endpoint and model both match.
    pub fn pick(&self, endpoint: &str, model: &str) -> Option<Arc<SchemaRule>> {
        self.rules
            .iter()
            .find(|r| {
                r.endpoint == endpoint
                    && r.model_pattern
                        .as_ref()
                        .map(|re| re.is_match(model))
                        .unwrap_or(true)
            })
            .cloned()
    }

    /// Validate a parsed JSON value against a rule.
    pub fn validate_value(rule: &SchemaRule, value: &Value) -> ValidationOutcome {
        let errors: Vec<String> = rule
            .schema
            .iter_errors(value)
            .map(|e| format!("{}: {}", e.instance_path, e))
            .collect();
        ValidationOutcome {
            passed: errors.is_empty(),
            errors,
            extracted: None,
        }
    }

    /// Validate a raw response string. Strips common LLM wrappers
    /// (markdown fences, leading prose) before parsing JSON.
    pub fn validate_response_text(rule: &SchemaRule, raw: &str) -> ValidationOutcome {
        let cleaned = extract_json_blob(raw);
        match serde_json::from_str::<Value>(&cleaned) {
            Ok(v) => {
                let mut out = Self::validate_value(rule, &v);
                if cleaned != raw {
                    out.extracted = Some(v);
                }
                out
            }
            Err(e) => ValidationOutcome {
                passed: false,
                errors: vec![format!("not valid JSON: {e}")],
                extracted: None,
            },
        }
    }
}

/// Build SchemaRules from a list of `(endpoint, model_pattern, schema_path, name)`.
pub fn load_rules(specs: &[RuleSpec]) -> Result<Vec<SchemaRule>> {
    let mut out = Vec::with_capacity(specs.len());
    for spec in specs {
        let raw = std::fs::read_to_string(&spec.schema_path)
            .with_context(|| format!("reading schema file {}", spec.schema_path))?;
        let schema_json: Value = serde_json::from_str(&raw)
            .with_context(|| format!("parsing schema JSON from {}", spec.schema_path))?;
        let validator = jsonschema::draft202012::new(&schema_json)
            .with_context(|| format!("compiling schema {}", spec.schema_path))?;
        let model_pattern = match &spec.model_pattern {
            Some(p) if !p.is_empty() => Some(
                Regex::new(p).with_context(|| format!("compiling model pattern {p}"))?,
            ),
            _ => None,
        };
        out.push(SchemaRule {
            endpoint: spec.endpoint.clone(),
            model_pattern,
            schema: validator,
            name: spec
                .name
                .clone()
                .unwrap_or_else(|| spec.schema_path.clone()),
        });
    }
    Ok(out)
}

/// Plain-data spec used at startup to build the validator.
#[derive(Debug, Clone)]
pub struct RuleSpec {
    pub endpoint: String,
    pub model_pattern: Option<String>,
    pub schema_path: String,
    pub name: Option<String>,
}

/// Strip common LLM wrappers from a response so JSON parsing has a chance:
/// - "Sure! Here is the JSON:\n" prose prefixes
/// - Markdown fenced blocks (```json ... ``` or just ``` ... ```)
/// - Trailing prose after a closing brace
fn extract_json_blob(raw: &str) -> String {
    let trimmed = raw.trim();

    // Markdown fence form: optional language tag, then the body.
    if let Some(rest) = trimmed.strip_prefix("```") {
        // Skip the "json" or "JSON" tag if present, plus the newline.
        let body_start = rest
            .find('\n')
            .map(|n| n + 1)
            .unwrap_or(0);
        let body = &rest[body_start..];
        if let Some(end) = body.rfind("```") {
            return body[..end].trim().to_string();
        }
    }

    // Look for the first { or [ and the last matching closing bracket. This
    // handles "Here is your JSON: { ... } Anything else?" cases.
    let open_idx = trimmed.find(|c| c == '{' || c == '[');
    let close_idx = trimmed.rfind(|c| c == '}' || c == ']');
    if let (Some(start), Some(end)) = (open_idx, close_idx) {
        if start < end {
            return trimmed[start..=end].to_string();
        }
    }

    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_validator(schema_json: &str) -> SchemaRule {
        let schema: Value = serde_json::from_str(schema_json).unwrap();
        let validator = jsonschema::draft202012::new(&schema).unwrap();
        SchemaRule {
            endpoint: "/v1/chat/completions".to_string(),
            model_pattern: None,
            schema: validator,
            name: "test".to_string(),
        }
    }

    #[test]
    fn passes_valid_object() {
        let rule = build_validator(
            r#"{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}"#,
        );
        let v = serde_json::json!({"name": "Alice"});
        let out = SchemaValidator::validate_value(&rule, &v);
        assert!(out.passed, "{:?}", out.errors);
    }

    #[test]
    fn fails_when_required_field_missing() {
        let rule = build_validator(
            r#"{"type":"object","required":["name"],"properties":{"name":{"type":"string"}}}"#,
        );
        let v = serde_json::json!({});
        let out = SchemaValidator::validate_value(&rule, &v);
        assert!(!out.passed);
        assert!(!out.errors.is_empty());
    }

    #[test]
    fn fails_on_wrong_type() {
        let rule = build_validator(
            r#"{"type":"object","properties":{"age":{"type":"integer"}}}"#,
        );
        let v = serde_json::json!({"age": "not a number"});
        let out = SchemaValidator::validate_value(&rule, &v);
        assert!(!out.passed);
    }

    #[test]
    fn extract_strips_markdown_fence() {
        let raw = "```json\n{\"name\":\"Alice\"}\n```";
        assert_eq!(extract_json_blob(raw), "{\"name\":\"Alice\"}");
    }

    #[test]
    fn extract_strips_leading_prose() {
        let raw = "Sure! Here is the JSON:\n{\"name\":\"Alice\"}\nLet me know if you need more.";
        assert_eq!(extract_json_blob(raw), "{\"name\":\"Alice\"}");
    }

    #[test]
    fn extract_handles_arrays() {
        let raw = "Here you go: [1, 2, 3]";
        assert_eq!(extract_json_blob(raw), "[1, 2, 3]");
    }

    #[test]
    fn validate_response_text_with_fenced_payload() {
        let rule = build_validator(
            r#"{"type":"object","required":["name"],"properties":{"name":{"type":"string"}}}"#,
        );
        let raw = "```json\n{\"name\":\"Alice\"}\n```";
        let out = SchemaValidator::validate_response_text(&rule, raw);
        assert!(out.passed);
        assert!(out.extracted.is_some(), "wrapper stripped → extracted set");
    }

    #[test]
    fn validate_response_text_with_invalid_json() {
        let rule = build_validator(r#"{"type":"object"}"#);
        let raw = "definitely not json";
        let out = SchemaValidator::validate_response_text(&rule, raw);
        assert!(!out.passed);
    }

    #[test]
    fn pick_returns_first_matching_rule() {
        let rule1 = SchemaRule {
            endpoint: "/v1/chat/completions".to_string(),
            model_pattern: Some(Regex::new("^gpt-4o").unwrap()),
            schema: jsonschema::draft202012::new(&serde_json::json!({})).unwrap(),
            name: "gpt".to_string(),
        };
        let rule2 = SchemaRule {
            endpoint: "/v1/chat/completions".to_string(),
            model_pattern: None, // any model
            schema: jsonschema::draft202012::new(&serde_json::json!({})).unwrap(),
            name: "default".to_string(),
        };
        let v = SchemaValidator::new(vec![rule1, rule2], ViolationAction::Reject);
        let picked = v
            .pick("/v1/chat/completions", "gpt-4o-mini")
            .expect("rule found");
        assert_eq!(picked.name, "gpt");
        let picked2 = v.pick("/v1/chat/completions", "claude-3").expect("default");
        assert_eq!(picked2.name, "default");
        assert!(v.pick("/v1/messages", "x").is_none());
    }
}
