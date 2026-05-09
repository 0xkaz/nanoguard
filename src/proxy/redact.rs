//! Entity-named PII redactor.
//!
//! Replaces matches with `[<ENTITY_NAME>]` (or indexed variants) placeholders.
//! Patterns are loaded from a `dicts/pii-regex.txt`-style file plus an inline
//! default set, so users can tune the entity list without recompiling.

use std::collections::HashMap;

use anyhow::{Context, Result};
use regex::{Regex, RegexBuilder};
use serde_json::Value;

/// How replacement placeholders are formatted.
///
/// - `Bare`: legacy `[<ENTITY>]` — stateless, fastest, but two emails collapse
///   to the same placeholder so the LLM cannot distinguish them.
/// - `Indexed`: `[<ENTITY>_<N>]` — same value reuses the same index within
///   one text scan; different values get different indices. Required step
///   toward reversible (Vault-backed) deanonymization.
/// - `LlmGuard`: `[REDACTED_<ENTITY>_<N>]` — wire-compatible with prompts
///   that follow the LLM Guard convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceholderStyle {
    Bare,
    Indexed,
    LlmGuard,
}

impl PlaceholderStyle {
    pub fn from_str(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "indexed" => PlaceholderStyle::Indexed,
            "llm_guard" | "llmguard" => PlaceholderStyle::LlmGuard,
            _ => PlaceholderStyle::Bare,
        }
    }
}

pub struct Redactor {
    rules: Vec<RedactRule>,
    style: PlaceholderStyle,
}

struct RedactRule {
    #[allow(dead_code)] // used in upcoming per-entity action map
    entity: String,
    placeholder: String, // pre-formatted bare placeholder; ignored for indexed styles
    regex: Regex,
}

impl Redactor {
    /// Build with the given inline (entity_name, regex_pattern) pairs and
    /// optional dictionary file paths. Each dict file is parsed in
    /// `/pattern/<TAB>entity_name` form (anything else is ignored — this
    /// reuses the existing dict loader convention but keys patterns by
    /// entity name, not by BLOCK/ALERT/FLAG).
    pub fn build(inline: &[(&str, &str)], dict_paths: &[String]) -> Result<Self> {
        Self::build_with_style(inline, dict_paths, PlaceholderStyle::Bare)
    }

    pub fn build_with_style(
        inline: &[(&str, &str)],
        dict_paths: &[String],
        style: PlaceholderStyle,
    ) -> Result<Self> {
        let mut rules = Vec::new();
        for (entity, pattern) in inline {
            rules.push(compile(entity, pattern)?);
        }
        for path in dict_paths {
            load_redactor_file(path, &mut rules)
                .with_context(|| format!("loading redactor file {path}"))?;
        }
        Ok(Self { rules, style })
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    pub fn style(&self) -> PlaceholderStyle {
        self.style
    }

    pub fn contains_pii(&self, text: &str) -> bool {
        self.rules.iter().any(|r| r.regex.is_match(text))
    }

    pub fn redact_text(&self, text: &str) -> String {
        match self.style {
            PlaceholderStyle::Bare => self.redact_text_bare(text),
            PlaceholderStyle::Indexed | PlaceholderStyle::LlmGuard => {
                self.redact_text_indexed(text)
            }
        }
    }

    fn redact_text_bare(&self, text: &str) -> String {
        let mut out = text.to_string();
        for rule in &self.rules {
            out = rule
                .regex
                .replace_all(&out, rule.placeholder.as_str())
                .into_owned();
        }
        out
    }

    /// Indexed redaction: assign a per-(entity, value) index inside one text
    /// scan. The same original value reuses the same index; different values
    /// for the same entity type get separate indices. Output format depends
    /// on `self.style` (Indexed vs LlmGuard).
    fn redact_text_indexed(&self, text: &str) -> String {
        let mut indexer = Indexer::new(self.style);
        // Walk rules in declaration order to preserve precedence (more specific
        // patterns listed first win). For each rule, find all matches and
        // substitute with a stable indexed placeholder.
        let mut out = text.to_string();
        for rule in &self.rules {
            // Collect all matches; replace from right to left so offsets stay valid.
            let matches: Vec<(usize, usize, String)> = rule
                .regex
                .find_iter(&out)
                .map(|m| (m.start(), m.end(), m.as_str().to_string()))
                .collect();
            for (start, end, value) in matches.into_iter().rev() {
                let placeholder = indexer.placeholder(&rule.entity, &value);
                out.replace_range(start..end, &placeholder);
            }
        }
        out
    }

    /// Walk a JSON request body and redact every `messages[].content` field,
    /// supporting both the OpenAI string form and the multi-part text array
    /// form. Returns true if any redaction occurred.
    pub fn redact_messages(&self, body: &mut Value) -> bool {
        let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
            return false;
        };
        let mut changed = false;
        for msg in messages {
            let Some(content) = msg.get_mut("content") else {
                continue;
            };
            if let Some(text) = content.as_str() {
                let redacted = self.redact_text(text);
                if redacted != text {
                    *content = Value::String(redacted);
                    changed = true;
                }
            } else if let Some(parts) = content.as_array_mut() {
                for part in parts {
                    let Some(text_val) = part.get_mut("text") else {
                        continue;
                    };
                    let Some(text) = text_val.as_str() else {
                        continue;
                    };
                    let redacted = self.redact_text(text);
                    if redacted != text {
                        *text_val = Value::String(redacted);
                        changed = true;
                    }
                }
            }
        }
        changed
    }
}

/// Per-text indexer that assigns stable placeholders to (entity, value) pairs.
struct Indexer {
    style: PlaceholderStyle,
    /// (entity, value) → assigned index, so duplicate values reuse one index.
    seen: HashMap<(String, String), u32>,
    /// entity → next free index counter.
    next: HashMap<String, u32>,
}

impl Indexer {
    fn new(style: PlaceholderStyle) -> Self {
        Self {
            style,
            seen: HashMap::new(),
            next: HashMap::new(),
        }
    }

    fn placeholder(&mut self, entity: &str, value: &str) -> String {
        let key = (entity.to_string(), value.to_string());
        let idx = if let Some(&i) = self.seen.get(&key) {
            i
        } else {
            let counter = self.next.entry(entity.to_string()).or_insert(0);
            *counter += 1;
            let assigned = *counter;
            self.seen.insert(key, assigned);
            assigned
        };
        match self.style {
            PlaceholderStyle::Indexed => format!("[{}_{}]", entity, idx),
            PlaceholderStyle::LlmGuard => format!("[REDACTED_{}_{}]", entity, idx),
            // Bare doesn't use the indexer; fall back defensively.
            PlaceholderStyle::Bare => format!("[{}]", entity),
        }
    }
}

fn compile(entity: &str, pattern: &str) -> Result<RedactRule> {
    let regex = RegexBuilder::new(pattern)
        .case_insensitive(false)
        .build()
        .with_context(|| format!("compiling redactor pattern for {entity}: `{pattern}`"))?;
    Ok(RedactRule {
        entity: entity.to_string(),
        placeholder: format!("[{}]", entity),
        regex,
    })
}

fn load_redactor_file(path: &str, rules: &mut Vec<RedactRule>) -> Result<()> {
    let content = std::fs::read_to_string(path)?;
    for (idx, raw_line) in content.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Format: /<pattern>/<TAB><entity_name>[<TAB>...]
        // Falls back to inferring from a leading tag comment line if no entity
        // column is given (legacy behaviour: skip lines without entity name).
        let mut cols = line.split('\t');
        let Some(pattern_field) = cols.next() else {
            continue;
        };
        let Some(stripped) = pattern_field
            .strip_prefix('/')
            .and_then(|s| s.rsplit_once('/'))
            .map(|(p, _)| p)
        else {
            continue;
        };
        if stripped.is_empty() {
            continue;
        }
        // Entity name comes from the second tab-separated column. The legacy
        // dict format puts a numeric BLOCK/ALERT/FLAG key there; we accept
        // both — if it's purely numeric we synthesize an ENTITY_<n> name.
        let entity_label = cols.next().unwrap_or("").trim();
        let entity = if entity_label.is_empty() || entity_label.parse::<u32>().is_ok() {
            format!("PII_{}", idx + 1)
        } else {
            entity_label.to_uppercase()
        };
        match compile(&entity, stripped) {
            Ok(rule) => rules.push(rule),
            Err(e) => tracing::warn!("{path}:{}: skip pattern: {e}", idx + 1),
        }
    }
    Ok(())
}

/// Default inline patterns matching the previous hardcoded set.
pub fn default_inline_patterns() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "EMAIL",
            r"\b[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}\b",
        ),
        ("SSN", r"\b\d{3}-\d{2}-\d{4}\b"),
        ("CARD", r"\b\d{4}[- ]?\d{4}[- ]?\d{4}[- ]?\d{4}\b"),
        ("AWS_ACCESS_KEY_ID", r"\bAKIA[0-9A-Z]{16}\b"),
        ("GITHUB_PAT", r"\bghp_[A-Za-z0-9]{36}\b"),
        (
            "GITHUB_FINE_GRAINED_PAT",
            r"\bgithub_pat_[A-Za-z0-9_]{82}\b",
        ),
        ("OPENAI_KEY", r"\bsk-[A-Za-z0-9]{20,}\b"),
        ("ANTHROPIC_KEY", r"\bsk-ant-[A-Za-z0-9_\-]{80,}\b"),
        ("STRIPE_KEY", r"\b(?:sk|rk)_live_[A-Za-z0-9]{20,}\b"),
        ("GOOGLE_API_KEY", r"\bAIza[0-9A-Za-z_\-]{35}\b"),
        (
            "JWT",
            r"\beyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\b",
        ),
        ("SLACK_TOKEN", r"\bxox[abpors]-[A-Za-z0-9-]{10,}\b"),
        ("TOKEN", r"\b[A-Za-z0-9_\-]{32,}\b"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn redactor() -> Redactor {
        Redactor::build(&default_inline_patterns(), &[]).unwrap()
    }

    #[test]
    fn redacts_email_with_entity_label() {
        let r = redactor();
        assert_eq!(r.redact_text("contact alice@example.com"), "contact [EMAIL]");
    }

    #[test]
    fn redacts_aws_access_key() {
        let r = redactor();
        let out = r.redact_text("creds: AKIAIOSFODNN7EXAMPLE end");
        assert_eq!(out, "creds: [AWS_ACCESS_KEY_ID] end");
    }

    #[test]
    fn redacts_github_pat() {
        let r = redactor();
        let out = r.redact_text("token ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa here");
        assert_eq!(out, "token [GITHUB_PAT] here");
    }

    #[test]
    fn redacts_jwt() {
        let r = redactor();
        let out = r.redact_text("auth eyJabc.eyJpYXQiOj.signature done");
        assert!(out.contains("[JWT]"), "got `{out}`");
    }

    #[test]
    fn redact_messages_handles_string_and_parts() {
        let r = redactor();
        let mut body = json!({
            "messages": [
                {"role": "user", "content": "ssn 123-45-6789"},
                {"role": "user", "content": [
                    {"type": "text", "text": "email me at alice@example.com"},
                    {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}
                ]}
            ]
        });
        assert!(r.redact_messages(&mut body));
        assert_eq!(body["messages"][0]["content"], "ssn [SSN]");
        assert_eq!(
            body["messages"][1]["content"][0]["text"],
            "email me at [EMAIL]"
        );
        assert_eq!(
            body["messages"][1]["content"][1]["image_url"]["url"],
            "https://example.com/a.png"
        );
    }

    #[test]
    fn contains_pii_returns_true_for_token() {
        let r = redactor();
        assert!(r.contains_pii("AKIAIOSFODNN7EXAMPLE"));
        assert!(!r.contains_pii("hello world"));
    }

    fn indexed_redactor() -> Redactor {
        Redactor::build_with_style(
            &default_inline_patterns(),
            &[],
            PlaceholderStyle::Indexed,
        )
        .unwrap()
    }

    fn llm_guard_redactor() -> Redactor {
        Redactor::build_with_style(
            &default_inline_patterns(),
            &[],
            PlaceholderStyle::LlmGuard,
        )
        .unwrap()
    }

    #[test]
    fn indexed_distinguishes_two_distinct_emails() {
        let r = indexed_redactor();
        let out = r.redact_text("contact alice@x.com or bob@y.com");
        assert!(out.contains("[EMAIL_1]"), "got `{out}`");
        assert!(out.contains("[EMAIL_2]"), "got `{out}`");
    }

    #[test]
    fn indexed_reuses_index_for_repeated_value() {
        let r = indexed_redactor();
        let out = r.redact_text("alice@x.com again alice@x.com");
        let count_1 = out.matches("[EMAIL_1]").count();
        assert_eq!(count_1, 2, "same value should reuse index 1, got `{out}`");
        assert!(!out.contains("[EMAIL_2]"));
    }

    #[test]
    fn indexed_separate_counters_per_entity() {
        let r = indexed_redactor();
        let out = r.redact_text("ssn 123-45-6789 email alice@x.com");
        assert!(out.contains("[SSN_1]"), "got `{out}`");
        assert!(out.contains("[EMAIL_1]"), "got `{out}`");
    }

    #[test]
    fn llm_guard_style_uses_redacted_prefix() {
        let r = llm_guard_redactor();
        let out = r.redact_text("contact alice@x.com");
        assert!(out.contains("[REDACTED_EMAIL_1]"), "got `{out}`");
    }

    #[test]
    fn loads_user_dict_file_with_entity_names() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "nanoguard-redact-{}.txt",
            std::process::id()
        ));
        std::fs::write(
            &path,
            "# user dict\n/\\bSEC-\\d{6}\\b/\tINTERNAL_TICKET\n",
        )
        .unwrap();

        let r = Redactor::build(&[], &[path.to_string_lossy().into_owned()]).unwrap();
        assert_eq!(
            r.redact_text("ref SEC-123456 here"),
            "ref [INTERNAL_TICKET] here"
        );
        let _ = std::fs::remove_file(path);
    }
}
