//! Restore original values from placeholders in LLM output text.
//!
//! Strategies are pluggable via the `MatchingStrategy` trait so future
//! additions (Fuzzy, Combined) drop in without changing call sites.
//!
//! Currently shipped:
//! - `Exact`: byte-exact placeholder match
//! - `CaseInsensitive`: ASCII case-insensitive match
//!
//! Roadmap (see `_RESEARCH_LLM_Guard.md`):
//! - `Fuzzy`: edit-distance ≤ 3 via fuzzy-matcher crate
//! - `Combined`: Exact then Fuzzy fallback

use crate::guard::vault::Vault;

pub trait MatchingStrategy: Send + Sync {
    fn restore(&self, output: &str, entries: &[(String, String)]) -> String;
}

/// Byte-exact placeholder substitution. The default — fastest and never
/// false-positives.
pub struct Exact;

impl MatchingStrategy for Exact {
    fn restore(&self, output: &str, entries: &[(String, String)]) -> String {
        let mut out = output.to_string();
        for (placeholder, original) in entries {
            if out.contains(placeholder.as_str()) {
                out = out.replace(placeholder.as_str(), original);
            }
        }
        out
    }
}

/// ASCII case-insensitive match. Useful when the LLM lowercases the
/// placeholder. Slightly slower than `Exact` due to per-rule scan.
pub struct CaseInsensitive;

impl MatchingStrategy for CaseInsensitive {
    fn restore(&self, output: &str, entries: &[(String, String)]) -> String {
        let mut out = output.to_string();
        for (placeholder, original) in entries {
            // Find placeholder ignoring ASCII case. We walk the string with a
            // sliding window; stop early if no candidates remain.
            let needle = placeholder.as_bytes();
            if needle.is_empty() {
                continue;
            }
            let mut buf = String::with_capacity(out.len());
            let bytes = out.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if i + needle.len() <= bytes.len()
                    && bytes[i..i + needle.len()].eq_ignore_ascii_case(needle)
                {
                    buf.push_str(original);
                    i += needle.len();
                } else {
                    // Push one char (handle UTF-8 safely by using char_indices on the slice).
                    let ch = out[i..].chars().next().unwrap();
                    buf.push(ch);
                    i += ch.len_utf8();
                }
            }
            out = buf;
        }
        out
    }
}

/// Build a strategy by name. Unknown names fall back to Exact with a warning.
pub fn strategy_from_name(name: &str) -> Box<dyn MatchingStrategy> {
    match name.to_ascii_lowercase().as_str() {
        "case_insensitive" | "case-insensitive" => Box::new(CaseInsensitive),
        "fuzzy" | "combined" => {
            tracing::warn!(
                "deanonymize strategy `{name}` is not yet implemented, falling back to Exact"
            );
            Box::new(Exact)
        }
        _ => Box::new(Exact),
    }
}

/// Convenience: run a strategy against a Vault directly.
pub fn restore<S: MatchingStrategy + ?Sized, V: Vault>(
    strategy: &S,
    vault: &V,
    output: &str,
) -> String {
    if vault.is_empty() {
        return output.to_string();
    }
    let entries = vault.entries();
    strategy.restore(output, &entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::vault::LocalVault;

    fn build_vault() -> LocalVault {
        let mut v = LocalVault::new();
        v.store("EMAIL", "alice@x.com");
        v.store("SSN", "123-45-6789");
        v
    }

    #[test]
    fn exact_restores_known_placeholder() {
        let v = build_vault();
        let out = restore(
            &Exact,
            &v,
            "your email is [REDACTED_EMAIL_1] and ssn [REDACTED_SSN_1]",
        );
        assert_eq!(out, "your email is alice@x.com and ssn 123-45-6789");
    }

    #[test]
    fn exact_leaves_unknown_placeholder() {
        let v = build_vault();
        let out = restore(&Exact, &v, "found [REDACTED_PERSON_99] mystery");
        assert!(out.contains("[REDACTED_PERSON_99]"));
    }

    #[test]
    fn empty_vault_returns_input_unchanged() {
        let v = LocalVault::new();
        let out = restore(&Exact, &v, "no vault entries here");
        assert_eq!(out, "no vault entries here");
    }

    #[test]
    fn case_insensitive_matches_lowercased_placeholder() {
        let v = build_vault();
        let out = restore(&CaseInsensitive, &v, "email = [redacted_email_1]");
        assert_eq!(out, "email = alice@x.com");
    }

    #[test]
    fn case_insensitive_handles_mixed_case() {
        let v = build_vault();
        let out = restore(&CaseInsensitive, &v, "[Redacted_Email_1] etc");
        assert_eq!(out, "alice@x.com etc");
    }

    #[test]
    fn strategy_from_name_returns_exact_for_unknown() {
        // Smoke test: fuzzy/combined currently fall back to Exact.
        let s = strategy_from_name("fuzzy");
        let v = build_vault();
        let out = restore(s.as_ref(), &v, "[REDACTED_EMAIL_1]");
        assert_eq!(out, "alice@x.com");
    }
}
