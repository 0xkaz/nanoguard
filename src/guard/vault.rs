//! Per-request Vault: maps PII placeholders to their original values so the
//! output deanonymizer can restore them after the LLM round-trip.
//!
//! Default scope is one Vault per HTTP request — created in the proxy handler,
//! consumed by Anonymize on input and Deanonymize on output, then dropped.
//! This is stateless and lock-free.
//!
//! Future scopes (Session, etc.) live behind the `Vault` trait so they can be
//! plugged in without touching call sites. See `_IDEA2.md` for the roadmap.

use std::collections::HashMap;

/// Vault contract. Implementations choose their own concurrency story.
pub trait Vault: Send + Sync {
    /// Insert (entity_type, original) and return the placeholder the caller
    /// should substitute into the prompt. Reuses an existing placeholder if
    /// the same (entity, original) tuple has been seen before.
    fn store(&mut self, entity_type: &str, original: &str) -> String;

    /// Look up the original value for a given placeholder string.
    fn get(&self, placeholder: &str) -> Option<&str>;

    /// Snapshot of all (placeholder, original) pairs — needed by fuzzy /
    /// case-insensitive deanonymize strategies that iterate every entry.
    fn entries(&self) -> Vec<(String, String)>;

    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Per-request Vault. Lock-free; not safe to share across threads — the proxy
/// handler owns one and threads its `&mut` reference through redaction.
pub struct LocalVault {
    /// placeholder → original
    entries: HashMap<String, String>,
    /// (entity_type, original) → assigned index, so duplicate (entity, value)
    /// reuses one placeholder across multiple `store` calls in the same request.
    seen: HashMap<(String, String), u32>,
    /// Per-entity-type next-free-index counter.
    next: HashMap<String, u32>,
    /// Placeholder format. The default LLM Guard-compatible form is
    /// `[REDACTED_<ENTITY>_<N>]`; alternatives can be added without touching
    /// the trait.
    style: PlaceholderTemplate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceholderTemplate {
    /// `[REDACTED_<ENTITY>_<N>]` — LLM Guard compatible (recommended).
    LlmGuard,
    /// `[<ENTITY>_<N>]` — shorter; useful when prompt budget is tight.
    Indexed,
}

impl LocalVault {
    pub fn new() -> Self {
        Self::with_style(PlaceholderTemplate::LlmGuard)
    }

    pub fn with_style(style: PlaceholderTemplate) -> Self {
        Self {
            entries: HashMap::new(),
            seen: HashMap::new(),
            next: HashMap::new(),
            style,
        }
    }

    fn format_placeholder(&self, entity: &str, idx: u32) -> String {
        match self.style {
            PlaceholderTemplate::LlmGuard => format!("[REDACTED_{}_{}]", entity, idx),
            PlaceholderTemplate::Indexed => format!("[{}_{}]", entity, idx),
        }
    }
}

impl Default for LocalVault {
    fn default() -> Self {
        Self::new()
    }
}

impl Vault for LocalVault {
    fn store(&mut self, entity_type: &str, original: &str) -> String {
        let key = (entity_type.to_string(), original.to_string());
        let idx = if let Some(&i) = self.seen.get(&key) {
            i
        } else {
            let counter = self.next.entry(entity_type.to_string()).or_insert(0);
            *counter += 1;
            let assigned = *counter;
            self.seen.insert(key, assigned);
            assigned
        };
        let placeholder = self.format_placeholder(entity_type, idx);
        // entries is keyed by placeholder; same placeholder maps to same original
        self.entries
            .entry(placeholder.clone())
            .or_insert_with(|| original.to_string());
        placeholder
    }

    fn get(&self, placeholder: &str) -> Option<&str> {
        self.entries.get(placeholder).map(String::as_str)
    }

    fn entries(&self) -> Vec<(String, String)> {
        self.entries
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_vault_is_empty() {
        let v = LocalVault::new();
        assert!(v.is_empty());
        assert_eq!(v.len(), 0);
    }

    #[test]
    fn stores_with_llm_guard_template_by_default() {
        let mut v = LocalVault::new();
        let p = v.store("EMAIL", "alice@x.com");
        assert_eq!(p, "[REDACTED_EMAIL_1]");
    }

    #[test]
    fn indexed_template_uses_short_form() {
        let mut v = LocalVault::with_style(PlaceholderTemplate::Indexed);
        let p = v.store("EMAIL", "alice@x.com");
        assert_eq!(p, "[EMAIL_1]");
    }

    #[test]
    fn same_value_reuses_index() {
        let mut v = LocalVault::new();
        let a = v.store("EMAIL", "alice@x.com");
        let b = v.store("EMAIL", "alice@x.com");
        assert_eq!(a, b);
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn different_values_get_different_indices() {
        let mut v = LocalVault::new();
        let a = v.store("EMAIL", "alice@x.com");
        let b = v.store("EMAIL", "bob@y.com");
        assert_ne!(a, b);
        assert_eq!(a, "[REDACTED_EMAIL_1]");
        assert_eq!(b, "[REDACTED_EMAIL_2]");
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn different_entities_have_separate_counters() {
        let mut v = LocalVault::new();
        let e = v.store("EMAIL", "alice@x.com");
        let s = v.store("SSN", "123-45-6789");
        assert_eq!(e, "[REDACTED_EMAIL_1]");
        assert_eq!(s, "[REDACTED_SSN_1]");
    }

    #[test]
    fn get_returns_original_for_known_placeholder() {
        let mut v = LocalVault::new();
        v.store("EMAIL", "alice@x.com");
        assert_eq!(v.get("[REDACTED_EMAIL_1]"), Some("alice@x.com"));
        assert_eq!(v.get("[REDACTED_EMAIL_99]"), None);
    }

    #[test]
    fn entries_snapshot_contains_all_pairs() {
        let mut v = LocalVault::new();
        v.store("EMAIL", "alice@x.com");
        v.store("SSN", "123-45-6789");
        let mut e = v.entries();
        e.sort();
        assert_eq!(e.len(), 2);
    }
}
