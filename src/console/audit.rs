//! Console audit log — records every administrative edit.
//!
//! Writes JSONL lines to the path configured in `[console].audit_path`.

use anyhow::{Context, Result};
use serde::Serialize;
use std::io::Write;
use std::sync::Mutex;

pub struct ConsoleAuditLog {
    file: Mutex<std::fs::File>,
}

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

    pub fn write_edit(&self, record: &EditRecord) -> Result<()> {
        let line = serde_json::to_string(record).context("serializing audit record")?;
        let mut file = self.file.lock().unwrap();
        writeln!(file, "{}", line).context("writing console audit log")?;
        file.sync_all().context("fsync console audit log")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
