use std::{
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::AuditConfig;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Allow,
    Block,
}

#[derive(Debug, Serialize)]
pub struct AuditEntry {
    pub request_id: String,
    pub timestamp: String,
    pub api_key: String,
    pub model: String,
    pub prompt_hash: String,
    pub verdict: Verdict,
    pub matched_rule: Option<String>,
    /// Policy rule id when the match was driven by a Policy bundle (e.g.
    /// "PI-001"). Always omitted from JSON when None for backward compat.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    /// Policy category (e.g. "prompt_injection", "pii", "off_topic").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Policy severity (e.g. "low" | "medium" | "high" | "critical").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    /// Compliance frameworks attached to the matched rule.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub compliance: Vec<String>,
    pub latency_us: u64,
}

pub struct AuditLog {
    file: Mutex<std::fs::File>,
    hash_only: bool,
}

impl AuditLog {
    pub fn open(cfg: &AuditConfig) -> anyhow::Result<Arc<Self>> {
        let path = Path::new(&cfg.path);
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Arc::new(Self {
            file: Mutex::new(file),
            hash_only: cfg.hash_only,
        }))
    }

    pub fn hash_prompt(&self, prompt: &str) -> String {
        let mut h = Sha256::new();
        h.update(prompt.as_bytes());
        format!("{:x}", h.finalize())
    }

    pub fn write(&self, entry: &AuditEntry) {
        let line = match serde_json::to_string(entry) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("audit serialize error: {e}");
                return;
            }
        };
        if let Ok(mut f) = self.file.lock() {
            let _ = writeln!(f, "{line}");
        }
    }

    /// Append a reload outcome to the audit log.
    ///
    /// Reload entries use a distinct verdict (`reload_ok` / `reload_failed`)
    /// and a slim schema. They share the file with request-level entries;
    /// consumers can filter on the `verdict` prefix.
    pub fn write_reload(&self, ok: bool, error: Option<String>, latency_us: u64) {
        let entry = ReloadEntry {
            request_id: new_request_id(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            verdict: if ok {
                ReloadVerdict::ReloadOk
            } else {
                ReloadVerdict::ReloadFailed
            },
            error,
            latency_us,
        };
        let line = match serde_json::to_string(&entry) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("audit serialize error (reload): {e}");
                return;
            }
        };
        if let Ok(mut f) = self.file.lock() {
            let _ = writeln!(f, "{line}");
        }
    }

    pub fn hash_only(&self) -> bool {
        self.hash_only
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
enum ReloadVerdict {
    ReloadOk,
    ReloadFailed,
}

#[derive(Debug, Serialize)]
struct ReloadEntry {
    request_id: String,
    timestamp: String,
    verdict: ReloadVerdict,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    latency_us: u64,
}

pub fn new_request_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // simple unique id: timestamp_ns in hex (no uuid dep)
    format!("{ns:032x}")
}
