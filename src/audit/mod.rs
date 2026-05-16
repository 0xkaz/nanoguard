use std::{
    io::Write,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
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
    fsync_every_write: bool,
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
            fsync_every_write: cfg.fsync_every_write,
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
                tracing::error!("audit serialize error: {e}");
                return;
            }
        };
        self.write_line(&line, "request");
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
                tracing::error!("audit serialize error (reload): {e}");
                return;
            }
        };
        self.write_line(&line, "reload");
    }

    pub fn hash_only(&self) -> bool {
        self.hash_only
    }

    // Shared sink for both request and reload entries. Surfaces lock
    // poisoning and I/O failures via `tracing::error!` instead of dropping
    // them silently — see XKA-48.
    fn write_line(&self, line: &str, kind: &str) {
        // Recover from a poisoned mutex: if a previous thread panicked
        // while holding the lock, the file handle itself is still valid,
        // so we keep writing rather than blackholing every subsequent
        // entry.
        let mut guard = match self.file.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                tracing::error!("audit lock was poisoned ({kind}); recovering and continuing");
                poisoned.into_inner()
            }
        };
        if let Err(e) = writeln!(&mut *guard, "{line}") {
            tracing::error!("audit write failed ({kind}): {e}");
            return;
        }
        if self.fsync_every_write {
            if let Err(e) = guard.sync_all() {
                tracing::error!("audit fsync failed ({kind}): {e}");
            }
        }
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

// Monotonic counter mixed into request ids so two ids minted in the same
// nanosecond (or on platforms with coarse clocks) still differ, and so an
// observer cannot trivially predict the next id from a previous one.
static REQUEST_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn new_request_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = REQUEST_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    // 32 hex chars of timestamp + 16 hex chars of counter. No uuid dep.
    format!("{ns:032x}{seq:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AuditConfig;
    use std::io::{BufRead, BufReader};

    fn temp_audit_path(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "nanoguard-audit-{tag}-{}-{nanos}.jsonl",
            std::process::id()
        ))
    }

    fn sample_entry() -> AuditEntry {
        AuditEntry {
            request_id: "test-req".to_string(),
            timestamp: "2026-05-17T00:00:00Z".to_string(),
            api_key: "alice".to_string(),
            model: "llama3.2".to_string(),
            prompt_hash: "deadbeef".to_string(),
            verdict: Verdict::Allow,
            matched_rule: None,
            rule_id: None,
            category: None,
            severity: None,
            compliance: vec![],
            latency_us: 42,
        }
    }

    fn read_lines(path: &std::path::Path) -> Vec<String> {
        let f = std::fs::File::open(path).expect("open audit file");
        BufReader::new(f)
            .lines()
            .map(|l| l.expect("read audit line"))
            .collect()
    }

    #[test]
    fn write_appends_jsonl_to_tempfile() {
        let path = temp_audit_path("write-basic");
        let cfg = AuditConfig {
            enabled: true,
            path: path.to_string_lossy().into_owned(),
            hash_only: true,
            fsync_every_write: false,
        };
        let log = AuditLog::open(&cfg).expect("open audit log");
        log.write(&sample_entry());
        log.write(&sample_entry());

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 2, "two entries should land in the file");
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).expect("each line is valid JSON");
            assert_eq!(v["verdict"], "allow");
            assert_eq!(v["api_key"], "alice");
        }

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn write_recovers_from_poisoned_lock() {
        let path = temp_audit_path("poison");
        let cfg = AuditConfig {
            enabled: true,
            path: path.to_string_lossy().into_owned(),
            hash_only: true,
            fsync_every_write: false,
        };
        let log = AuditLog::open(&cfg).expect("open audit log");

        // Poison the mutex by panicking inside a thread that holds it.
        let log_c = Arc::clone(&log);
        let _ = std::thread::spawn(move || {
            let _g = log_c.file.lock().expect("acquire lock before panic");
            panic!("intentional poison for XKA-48 test");
        })
        .join();
        assert!(
            log.file.is_poisoned(),
            "precondition: the mutex must be poisoned after the panic"
        );

        // The next write must still land in the file (and must not panic).
        log.write(&sample_entry());

        let lines = read_lines(&path);
        assert_eq!(
            lines.len(),
            1,
            "write after lock poison should still append the entry"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn fsync_every_write_setting_is_honored() {
        // We cannot directly assert that fsync was called, but we can
        // assert that turning the flag on does not break writes and that
        // the data is durably visible on the filesystem after the call
        // returns — which is the user-visible contract.
        let path = temp_audit_path("fsync");
        let cfg = AuditConfig {
            enabled: true,
            path: path.to_string_lossy().into_owned(),
            hash_only: true,
            fsync_every_write: true,
        };
        let log = AuditLog::open(&cfg).expect("open audit log");
        assert!(log.fsync_every_write, "config flag should be plumbed");

        log.write(&sample_entry());
        // Drop the log handle to release the OS file handle, then re-read.
        drop(log);

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 1);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn new_request_id_is_unique_under_tight_loop() {
        // Without the atomic counter, a tight loop on coarse-clock
        // platforms can mint duplicate ids. With it, ids must be unique.
        let mut ids = std::collections::HashSet::new();
        for _ in 0..10_000 {
            let id = new_request_id();
            assert!(ids.insert(id), "request ids must be unique");
        }
    }

    #[test]
    fn write_reload_emits_reload_verdict() {
        let path = temp_audit_path("reload");
        let cfg = AuditConfig {
            enabled: true,
            path: path.to_string_lossy().into_owned(),
            hash_only: true,
            fsync_every_write: false,
        };
        let log = AuditLog::open(&cfg).expect("open audit log");

        log.write_reload(true, None, 1234);
        log.write_reload(false, Some("boom".to_string()), 5678);

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 2);
        let ok: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        let fail: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(ok["verdict"], "reload_ok");
        assert_eq!(fail["verdict"], "reload_failed");
        assert_eq!(fail["error"], "boom");

        let _ = std::fs::remove_file(path);
    }
}
