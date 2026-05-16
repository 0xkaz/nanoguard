//! Console audit log — records every administrative mutation.
//!
//! Writes JSONL lines to the path configured in `[console].audit_path`. This
//! is the **after-the-fact accountability** surface for the console: who did
//! what, to whom, when, with what before/after state.
//!
//! Two record shapes share the file. They are distinguished by the `action`
//! field; consumers should always read `action` first and dispatch on it.
//!
//! - File-edit actions (`edit`, `revert`) carry `file`, `before_hash`,
//!   `after_hash`, and a free-form `summary`. These were the original Phase 1
//!   shape (see [`EditRecord`]).
//! - User / token / session mutations (`login`, `logout`, `token_create`,
//!   `token_revoke`, `user_create`, `user_update`, `user_role_change`,
//!   `user_force_revoke_all`) carry `target` plus `before` / `after` JSON
//!   snapshots of the changed fields.
//!
//! Both shapes share `actor`, `actor_id`, `timestamp`, `action`, `summary`,
//! and `request_id` — the common envelope. Field additions are non-breaking;
//! consumers must treat unknown actions as opaque.
//!
//! The proxy does not write to this file. The console does not write to the
//! proxy's audit log.

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::io::Write;
use std::sync::Mutex;

use crate::audit::new_request_id;

/// Persistent handle to the console-audit JSONL file.
pub struct ConsoleAuditLog {
    file: Mutex<std::fs::File>,
}

/// The original Phase 1 record shape — kept for backward compatibility with
/// the on-disk format of pre-XKA-61 console audit files.
///
/// New call sites should prefer [`MutationRecord`], which carries the full
/// `{actor, actor_id, target, before, after, request_id}` envelope.
#[derive(Debug, Serialize)]
pub struct EditRecord {
    pub timestamp: String,
    pub actor: String,
    pub action: String,
    pub file: String,
    pub before_hash: String,
    pub after_hash: String,
    pub summary: String,
}

/// Console mutation record.
///
/// Schema (per `docs/design/web-config-ui.md` § Threat model):
///
/// ```json
/// {
///   "request_id": "…",
///   "timestamp":  "2026-05-17T14:32:00Z",
///   "actor":      "alice",
///   "actor_id":   "7",
///   "action":     "user_update",
///   "target":     "carol",
///   "before":     { "allowed_models": ["X"] },
///   "after":      { "allowed_models": ["X", "Y"] },
///   "summary":    "admin alice changed allowed_models for carol"
/// }
/// ```
///
/// `before` and `after` are intentionally typed as `serde_json::Value` so each
/// action can carry exactly the fields it changed without forcing a fixed
/// schema across all action kinds. Both are omitted from JSON when null
/// (e.g. a `login` carries neither).
#[derive(Debug, Serialize)]
pub struct MutationRecord {
    pub request_id: String,
    pub timestamp: String,
    pub actor: String,
    /// Actor id — the users.id of the authenticated session. Stringified so
    /// future OIDC subjects (which are strings) can share the field.
    pub actor_id: String,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<JsonValue>,
    pub summary: String,
}

impl MutationRecord {
    /// Build a record with `request_id` and `timestamp` filled in.
    pub fn new(actor: &str, actor_id: i64, action: &str, summary: impl Into<String>) -> Self {
        Self {
            request_id: new_request_id(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            actor: actor.to_string(),
            actor_id: actor_id.to_string(),
            action: action.to_string(),
            target: None,
            before: None,
            after: None,
            summary: summary.into(),
        }
    }

    pub fn with_target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }

    pub fn with_before(mut self, before: JsonValue) -> Self {
        self.before = Some(before);
        self
    }

    pub fn with_after(mut self, after: JsonValue) -> Self {
        self.after = Some(after);
        self
    }
}

impl ConsoleAuditLog {
    pub fn open(path: &str) -> Result<Self> {
        let p = std::path::Path::new(path);
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .with_context(|| format!("opening console audit log {}", path))?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    /// Append a legacy file-edit record. Existing call sites for `edit` /
    /// `revert` continue to use this shape so the on-disk JSONL stays
    /// compatible.
    pub fn write_edit(&self, record: &EditRecord) -> Result<()> {
        let line = serde_json::to_string(record).context("serializing audit record")?;
        self.write_line(&line)
    }

    /// Append a richer mutation record (login, token, user, etc.).
    pub fn write_mutation(&self, record: &MutationRecord) -> Result<()> {
        let line = serde_json::to_string(record).context("serializing audit record")?;
        self.write_line(&line)
    }

    /// Best-effort write that logs (but does not propagate) failures. Hooks
    /// in the request path call this to avoid turning a successful mutation
    /// into a user-visible 500 just because the audit log is unwritable.
    pub fn record_mutation(&self, record: &MutationRecord) {
        if let Err(e) = self.write_mutation(record) {
            tracing::warn!("console audit log: failed to write mutation: {}", e);
        }
    }

    fn write_line(&self, line: &str) -> Result<()> {
        let mut file = self.file.lock().unwrap_or_else(|p| p.into_inner());
        writeln!(file, "{}", line).context("writing console audit log")?;
        file.sync_all().context("fsync console audit log")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nanoguard-audit-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn cleanup(dir: &std::path::Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn writes_edit_record() {
        let dir = tmp_dir();
        let path = dir.join("audit.jsonl");
        let log = ConsoleAuditLog::open(path.to_str().unwrap()).unwrap();
        log.write_edit(&EditRecord {
            timestamp: chrono::Utc::now().to_rfc3339(),
            actor: "admin".to_string(),
            action: "edit".to_string(),
            file: "dicts/test.txt".to_string(),
            before_hash: "abc".to_string(),
            after_hash: "def".to_string(),
            summary: "added 2 rules".to_string(),
        })
        .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("admin"));
        assert!(content.contains("dicts/test.txt"));
        cleanup(&dir);
    }

    #[test]
    fn writes_mutation_record_with_full_envelope() {
        let dir = tmp_dir();
        let path = dir.join("audit.jsonl");
        let log = ConsoleAuditLog::open(path.to_str().unwrap()).unwrap();

        let rec = MutationRecord::new("alice", 7, "user_update", "changed allowed_models")
            .with_target("carol")
            .with_before(json!({ "allowed_models": "[\"X\"]" }))
            .with_after(json!({ "allowed_models": "[\"X\", \"Y\"]" }));
        log.write_mutation(&rec).unwrap();

        let line = std::fs::read_to_string(&path).unwrap();
        let v: JsonValue = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["actor"], "alice");
        assert_eq!(v["actor_id"], "7");
        assert_eq!(v["action"], "user_update");
        assert_eq!(v["target"], "carol");
        assert_eq!(v["before"]["allowed_models"], "[\"X\"]");
        assert_eq!(v["after"]["allowed_models"], "[\"X\", \"Y\"]");
        assert!(v["request_id"].as_str().unwrap().len() == 48);
        assert!(v["timestamp"].as_str().unwrap().contains('T'));
        cleanup(&dir);
    }

    #[test]
    fn mutation_record_omits_unset_fields() {
        // A login carries neither before nor after; the JSON must not include
        // them as null, which would confuse downstream filters.
        let dir = tmp_dir();
        let path = dir.join("audit.jsonl");
        let log = ConsoleAuditLog::open(path.to_str().unwrap()).unwrap();

        let rec = MutationRecord::new("bob", 3, "login", "bob logged in");
        log.write_mutation(&rec).unwrap();

        let line = std::fs::read_to_string(&path).unwrap();
        let v: JsonValue = serde_json::from_str(line.trim()).unwrap();
        assert!(v.get("target").is_none());
        assert!(v.get("before").is_none());
        assert!(v.get("after").is_none());
        cleanup(&dir);
    }

    #[test]
    fn record_mutation_swallows_io_failure() {
        // Best-effort writes must not panic even if the underlying file went
        // away. We can't easily delete the open fd, but we can confirm that
        // record_mutation returns `()` and continues to write subsequent
        // records when the file is intact.
        let dir = tmp_dir();
        let path = dir.join("audit.jsonl");
        let log = ConsoleAuditLog::open(path.to_str().unwrap()).unwrap();
        log.record_mutation(&MutationRecord::new("a", 1, "login", "ok"));
        log.record_mutation(&MutationRecord::new("a", 1, "logout", "ok"));
        let line_count = std::fs::read_to_string(&path).unwrap().lines().count();
        assert_eq!(line_count, 2);
        cleanup(&dir);
    }
}
