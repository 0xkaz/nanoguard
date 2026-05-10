//! Policy bundles — declarative rule definitions for the input pipeline.
//!
//! A `Policy` is a versioned collection of `Rule`s loaded from a YAML file.
//! Each rule has a stable id, a category, a severity, and an action. The
//! audit log carries those metadata fields, so SIEM and compliance tools
//! can answer "why was this blocked / redacted / flagged" with the rule
//! id rather than just a matched string.
//!
//! Scope of v1 (v0.7.0):
//! - Keyword rules (literal phrases, action: block / alert / flag).
//! - PII regex rules (entity-named, action: redact / reject / log).
//!
//! Out of scope for v1, deliberately:
//! - Spotlight, Tool Gate, Schema rules (still configured via TOML).
//! - Hot reload / signed bundles / industry pack distribution.
//!
//! The bundle is loaded once at startup. Policy rules complement (do not
//! replace) the existing TOML inline lists — both feed into the same
//! matcher / redactor at build time. See _POLICY_ENGINE.md for the
//! design rationale.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Policy {
    /// Schema version — used by future loaders to refuse incompatible
    /// bundles. Currently must be `1`.
    pub version: u32,
    #[serde(default)]
    pub metadata: PolicyMetadata,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PolicyMetadata {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub vendor: Option<String>,
    #[serde(default)]
    pub created: Option<String>,
    #[serde(default)]
    pub updated: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    pub id: String,
    pub category: String,
    #[serde(default = "default_severity")]
    pub severity: String,
    /// Pattern body. If wrapped in `/.../`, treated as a regex; otherwise
    /// as a literal multi-word keyword (case-insensitive after normalize).
    pub pattern: String,
    pub action: RuleAction,
    /// Required for `action = "redact"`: the entity tag used to build the
    /// placeholder (`[<placeholder>]` or `[<placeholder>_<n>]`).
    #[serde(default)]
    pub placeholder: Option<String>,
    /// Optional human-readable description.
    #[serde(default)]
    pub message: Option<String>,
    /// Optional list of compliance frameworks the rule maps to (e.g.
    /// `["HIPAA", "GDPR"]`). Surfaced in audit log entries.
    #[serde(default)]
    pub compliance: Vec<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    Block,
    Alert,
    Flag,
    Redact,
    Reject,
    Log,
}

fn default_severity() -> String {
    "medium".to_string()
}

impl Policy {
    pub fn from_yaml_str(s: &str) -> Result<Self> {
        let policy: Self = serde_yaml::from_str(s).context("parsing policy YAML")?;
        policy.validate()?;
        Ok(policy)
    }

    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading policy bundle {:?}", path.as_ref()))?;
        Self::from_yaml_str(&raw)
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1 {
            anyhow::bail!("unsupported policy version {} (expected 1)", self.version);
        }
        let mut seen_ids = std::collections::HashSet::new();
        for rule in &self.rules {
            if rule.id.is_empty() {
                anyhow::bail!("rule has empty id");
            }
            if !seen_ids.insert(rule.id.clone()) {
                anyhow::bail!("duplicate rule id `{}`", rule.id);
            }
            if rule.pattern.is_empty() {
                anyhow::bail!("rule `{}` has empty pattern", rule.id);
            }
            if rule.action == RuleAction::Redact && rule.placeholder.is_none() {
                anyhow::bail!(
                    "rule `{}` action=redact requires `placeholder` field",
                    rule.id
                );
            }
        }
        Ok(())
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }
}

/// Return true if a pattern string is wrapped in `/.../` (regex form).
pub fn is_regex_pattern(p: &str) -> bool {
    p.len() >= 2 && p.starts_with('/') && p.ends_with('/')
}

/// Strip `/.../` to get the inner regex body.
pub fn regex_body(p: &str) -> Option<&str> {
    p.strip_prefix('/').and_then(|s| s.strip_suffix('/'))
}

/// Lightweight metadata snapshot for a single rule, used by the audit log
/// to enrich match records with category / severity / rule id without
/// having to reach back into the Policy at log time.
#[derive(Debug, Clone)]
pub struct RuleMeta {
    pub id: String,
    pub category: String,
    pub severity: String,
    pub compliance: Vec<String>,
}

impl RuleMeta {
    pub fn from_rule(rule: &Rule) -> Self {
        Self {
            id: rule.id.clone(),
            category: rule.category.clone(),
            severity: rule.severity.clone(),
            compliance: rule.compliance.clone(),
        }
    }
}

/// Lookup table: matched-pattern (literal) or regex body → RuleMeta.
/// The matcher / redactor return the matched string; the audit layer
/// uses this index to find the rule that produced it.
#[derive(Debug, Clone, Default)]
pub struct PolicyRuleIndex {
    /// Lower-cased literal pattern → meta. Lookup uses `to_ascii_lowercase`
    /// to mirror the matcher's case-insensitive normalize.
    by_literal: std::collections::HashMap<String, RuleMeta>,
    /// Regex body (without `/.../`) → meta. Lookup is exact: the matcher
    /// surfaces the rule's regex source string in audit metadata, so a
    /// direct map by the body string is sufficient.
    by_regex: std::collections::HashMap<String, RuleMeta>,
    /// Placeholder name (e.g. EMAIL) → meta, for redact rules.
    by_placeholder: std::collections::HashMap<String, RuleMeta>,
}

impl PolicyRuleIndex {
    pub fn from_policy(policy: &Policy) -> Self {
        let mut idx = PolicyRuleIndex::default();
        for rule in &policy.rules {
            let meta = RuleMeta::from_rule(rule);
            if let Some(body) = regex_body(&rule.pattern) {
                idx.by_regex.insert(body.to_string(), meta.clone());
                if let Some(p) = &rule.placeholder {
                    idx.by_placeholder.insert(p.clone(), meta);
                }
            } else {
                idx.by_literal
                    .insert(rule.pattern.to_ascii_lowercase(), meta);
            }
        }
        idx
    }

    pub fn lookup_literal(&self, matched_text: &str) -> Option<&RuleMeta> {
        self.by_literal.get(&matched_text.to_ascii_lowercase())
    }

    pub fn lookup_regex(&self, regex_body: &str) -> Option<&RuleMeta> {
        self.by_regex.get(regex_body)
    }

    pub fn lookup_placeholder(&self, placeholder: &str) -> Option<&RuleMeta> {
        self.by_placeholder.get(placeholder)
    }

    pub fn is_empty(&self) -> bool {
        self.by_literal.is_empty() && self.by_regex.is_empty() && self.by_placeholder.is_empty()
    }
}

/// Distribute a Policy's rules across the existing matcher and redactor
/// configuration by mutating the corresponding inline collections in
/// `keyword` and the inline pattern list passed for the redactor.
///
/// This is intentionally a *merge*: existing TOML inline rules are kept,
/// and policy rules are added on top. The matcher / redactor builders
/// don't need to know that some of their patterns came from a Policy.
pub fn merge_into_keyword_config(policy: &Policy, keyword: &mut crate::config::KeywordConfig) {
    for rule in &policy.rules {
        // Only literal keyword rules feed into matcher.inline_*.
        if regex_body(&rule.pattern).is_some() {
            continue;
        }
        let bucket = match rule.action {
            RuleAction::Block => &mut keyword.inline_block,
            RuleAction::Alert => &mut keyword.inline_alert,
            RuleAction::Flag => &mut keyword.inline_flag,
            // Redact / Reject / Log on a literal pattern doesn't really
            // make sense for keywords; ignore for now and document.
            RuleAction::Redact | RuleAction::Reject | RuleAction::Log => continue,
        };
        bucket.push(rule.pattern.clone());
    }
}

/// Distribute a Policy's regex/redact rules into a list of
/// `(entity_name, regex_body)` tuples suitable for the existing
/// `Redactor::build_with_style` / `default_inline_patterns` shape.
pub fn redactor_patterns_from_policy(policy: &Policy) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for rule in &policy.rules {
        if rule.action != RuleAction::Redact {
            continue;
        }
        let Some(body) = regex_body(&rule.pattern) else {
            continue;
        };
        let Some(placeholder) = &rule.placeholder else {
            continue;
        };
        out.push((placeholder.clone(), body.to_string()));
    }
    out
}

#[cfg(test)]
mod merge_tests {
    use super::*;
    use crate::config::KeywordConfig;

    fn block_policy() -> Policy {
        Policy::from_yaml_str(
            r#"
version: 1
rules:
  - id: PI-1
    category: prompt_injection
    severity: high
    pattern: ignore previous instructions
    action: block
  - id: PI-2
    category: prompt_injection
    severity: medium
    pattern: jailbreak
    action: block
  - id: OFF-1
    category: off_topic
    severity: low
    pattern: bitcoin
    action: flag
"#,
        )
        .unwrap()
    }

    #[test]
    fn merge_appends_keyword_rules_to_buckets() {
        let policy = block_policy();
        let mut kw = KeywordConfig::default();
        let original_block_len = kw.inline_block.len();
        merge_into_keyword_config(&policy, &mut kw);
        // 2 block rules + 1 flag rule
        assert_eq!(kw.inline_block.len(), original_block_len + 2);
        assert!(kw.inline_block.contains(&"jailbreak".to_string()));
    }

    #[test]
    fn redactor_patterns_extracts_redact_regex_rules() {
        let policy = Policy::from_yaml_str(
            r#"
version: 1
rules:
  - id: PII-1
    category: pii
    severity: medium
    pattern: '/[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}/'
    action: redact
    placeholder: EMAIL
  - id: PII-2
    category: pii
    severity: critical
    pattern: '/AKIA[0-9A-Z]{16}/'
    action: redact
    placeholder: AWS_ACCESS_KEY_ID
"#,
        )
        .unwrap();
        let pats = redactor_patterns_from_policy(&policy);
        assert_eq!(pats.len(), 2);
        assert_eq!(pats[0].0, "EMAIL");
        assert!(pats[0].1.contains('@'));
        assert_eq!(pats[1].0, "AWS_ACCESS_KEY_ID");
    }

    #[test]
    fn rule_index_lookups() {
        let policy = block_policy();
        let idx = PolicyRuleIndex::from_policy(&policy);
        assert_eq!(
            idx.lookup_literal("ignore previous instructions")
                .map(|m| m.id.as_str()),
            Some("PI-1")
        );
        assert_eq!(
            idx.lookup_literal("JAILBREAK").map(|m| m.id.as_str()),
            Some("PI-2"),
            "lookup is case-insensitive"
        );
        assert!(idx.lookup_literal("not in policy").is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_policy() {
        let yaml = r#"
version: 1
rules:
  - id: PI-001
    category: prompt_injection
    severity: high
    pattern: ignore previous instructions
    action: block
"#;
        let p = Policy::from_yaml_str(yaml).expect("valid policy");
        assert_eq!(p.version, 1);
        assert_eq!(p.rules.len(), 1);
        assert_eq!(p.rules[0].action, RuleAction::Block);
    }

    #[test]
    fn parses_redact_rule_with_placeholder() {
        let yaml = r#"
version: 1
rules:
  - id: PII-001
    category: pii
    severity: medium
    pattern: '/[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}/'
    action: redact
    placeholder: EMAIL
"#;
        let p = Policy::from_yaml_str(yaml).expect("valid policy");
        assert_eq!(p.rules[0].placeholder.as_deref(), Some("EMAIL"));
    }

    #[test]
    fn rejects_unknown_version() {
        let yaml = r#"
version: 99
rules: []
"#;
        let err = Policy::from_yaml_str(yaml).unwrap_err();
        assert!(err.to_string().contains("unsupported policy version"));
    }

    #[test]
    fn rejects_duplicate_rule_ids() {
        let yaml = r#"
version: 1
rules:
  - id: A
    category: x
    pattern: foo
    action: block
  - id: A
    category: x
    pattern: bar
    action: block
"#;
        let err = Policy::from_yaml_str(yaml).unwrap_err();
        assert!(err.to_string().contains("duplicate rule id"));
    }

    #[test]
    fn rejects_redact_without_placeholder() {
        let yaml = r#"
version: 1
rules:
  - id: PII-X
    category: pii
    pattern: '/foo/'
    action: redact
"#;
        let err = Policy::from_yaml_str(yaml).unwrap_err();
        assert!(err.to_string().contains("requires `placeholder`"));
    }

    #[test]
    fn detects_regex_vs_literal() {
        assert!(is_regex_pattern("/foo/"));
        assert!(!is_regex_pattern("foo"));
        assert_eq!(regex_body("/abc/"), Some("abc"));
        assert_eq!(regex_body("abc"), None);
    }

    #[test]
    fn metadata_fields_are_optional() {
        let yaml = r#"
version: 1
metadata:
  name: Test
rules: []
"#;
        let p = Policy::from_yaml_str(yaml).unwrap();
        assert_eq!(p.metadata.name.as_deref(), Some("Test"));
        assert!(p.metadata.vendor.is_none());
    }

    #[test]
    fn default_severity_is_medium() {
        let yaml = r#"
version: 1
rules:
  - id: X
    category: c
    pattern: p
    action: block
"#;
        let p = Policy::from_yaml_str(yaml).unwrap();
        assert_eq!(p.rules[0].severity, "medium");
    }
}
