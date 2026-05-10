//! Guard pipeline components — Vault, Anonymize, Deanonymize, etc.
//!
//! Currently this module hosts the Vault primitive used by reversible PII
//! redaction. Future additions: Anonymizer entry point, Fuzzy/Combined
//! deanonymize strategies, session-scoped vaults.

pub mod deanonymize;
pub mod schema;
pub mod spotlight;
pub mod sse_deanon;
pub mod tool_gate;
pub mod vault;
