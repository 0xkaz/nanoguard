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

    pub fn hash_only(&self) -> bool {
        self.hash_only
    }
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
