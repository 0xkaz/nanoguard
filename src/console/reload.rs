//! Reload trigger for the nanoguard console.
//!
//! Sends reload signals to the proxy process and polls the proxy audit log
//! for the outcome.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::time::{Duration, SystemTime};

/// Outcome of a reload trigger attempt.
#[derive(Debug, Clone, Serialize)]
pub struct ReloadOutcome {
    pub triggered: bool,
    pub method: String,
    pub error: Option<String>,
}

/// Trigger a reload via the configured method.
pub fn trigger_reload(cfg: &crate::config::ReloadConfig) -> ReloadOutcome {
    // Prefer Unix socket if configured.
    if let Some(ref socket_path) = cfg.socket {
        match trigger_via_socket(socket_path) {
            Ok(()) => ReloadOutcome {
                triggered: true,
                method: "socket".to_string(),
                error: None,
            },
            Err(e) => ReloadOutcome {
                triggered: false,
                method: "socket".to_string(),
                error: Some(format!("{e:#}")),
            },
        }
    } else if let Some(ref pid_file) = cfg.pid_file {
        match trigger_via_pid_file(pid_file) {
            Ok(()) => ReloadOutcome {
                triggered: true,
                method: "sighup".to_string(),
                error: None,
            },
            Err(e) => ReloadOutcome {
                triggered: false,
                method: "sighup".to_string(),
                error: Some(format!("{e:#}")),
            },
        }
    } else {
        ReloadOutcome {
            triggered: false,
            method: "none".to_string(),
            error: Some(
                "no reload method configured (set reload.pid_file or reload.socket)".to_string(),
            ),
        }
    }
}

#[cfg(unix)]
fn trigger_via_pid_file(pid_file: &str) -> Result<()> {
    let pid_str = std::fs::read_to_string(pid_file)
        .with_context(|| format!("reading pid file {}", pid_file))?;
    let pid: i32 = pid_str
        .trim()
        .parse()
        .with_context(|| format!("parsing PID from {}", pid_file))?;
    unsafe {
        if libc::kill(pid, libc::SIGHUP) != 0 {
            let err = std::io::Error::last_os_error();
            bail!("failed to send SIGHUP to PID {}: {}", pid, err);
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn trigger_via_pid_file(_pid_file: &str) -> Result<()> {
    bail!("SIGHUP reload is only supported on Unix")
}

#[cfg(unix)]
fn trigger_via_socket(socket_path: &str) -> Result<()> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(socket_path)
        .with_context(|| format!("connecting to reload socket {}", socket_path))?;
    stream
        .write_all(b"RELOAD\n")
        .with_context(|| "writing RELOAD to socket")?;
    stream.flush().with_context(|| "flushing socket")?;

    let mut buf = [0u8; 256];
    let n = stream
        .read(&mut buf)
        .with_context(|| "reading socket response")?;
    let resp = String::from_utf8_lossy(&buf[..n]);
    if resp.starts_with("OK") {
        Ok(())
    } else if resp.starts_with("ERR") {
        bail!("proxy returned error: {}", resp.trim());
    } else {
        bail!("unexpected proxy response: {}", resp.trim());
    }
}

#[cfg(not(unix))]
fn trigger_via_socket(_socket_path: &str) -> Result<()> {
    bail!("Unix socket reload is only supported on Unix")
}

/// Poll the proxy audit log for a reload outcome newer than `after`.
///
/// Returns `Some(true)` for reload_ok, `Some(false)` for reload_failed,
/// or `None` if no entry was found within the poll window.
pub fn poll_reload_status(audit_path: &str, after: SystemTime, max_lines: usize) -> Option<bool> {
    let content = match std::fs::read_to_string(audit_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("poll_reload_status: read failed: {}", e);
            return None;
        }
    };

    let after_secs = after
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    // Scan newest lines first.
    for line in content.lines().rev().take(max_lines) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(ts) = obj.get("timestamp").and_then(|v| v.as_str()) else {
            continue;
        };
        // Parse RFC3339; if it fails, skip.
        let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) else {
            continue;
        };
        let line_secs = dt.timestamp() as f64 + dt.timestamp_subsec_nanos() as f64 / 1e9;
        if line_secs <= after_secs {
            continue; // too old
        }
        let verdict = obj.get("verdict").and_then(|v| v.as_str());
        match verdict {
            Some("reload_ok") => return Some(true),
            Some("reload_failed") => return Some(false),
            _ => continue,
        }
    }
    None
}

/// Poll with retries, returning the final status or timeout.
pub async fn wait_for_reload_status(
    audit_path: &str,
    after: SystemTime,
    retries: usize,
    delay_ms: u64,
) -> ReloadPollResult {
    for _ in 0..retries {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        if let Some(ok) = poll_reload_status(audit_path, after, 200) {
            return ReloadPollResult {
                ready: true,
                ok: Some(ok),
                error: None,
            };
        }
    }
    ReloadPollResult {
        ready: false,
        ok: None,
        error: Some("timeout waiting for reload outcome".to_string()),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ReloadPollResult {
    pub ready: bool,
    pub ok: Option<bool>,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nanoguard-reload-test-{}-{}",
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
    fn poll_reload_status_finds_ok() {
        let dir = tmp_dir();
        let path = dir.join("audit.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        let t0 = SystemTime::now();
        writeln!(
            f,
            r#"{{"timestamp":"2020-01-01T00:00:00Z","verdict":"reload_ok"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"timestamp":"{}","verdict":"reload_failed"}}"#,
            chrono::Utc::now().to_rfc3339()
        )
        .unwrap();
        let result = poll_reload_status(path.to_str().unwrap(), t0, 10);
        assert_eq!(result, Some(false));
        cleanup(&dir);
    }

    #[test]
    fn poll_reload_status_ignores_old_entries() {
        let dir = tmp_dir();
        let path = dir.join("audit.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        let t0 = SystemTime::now();
        writeln!(
            f,
            r#"{{"timestamp":"2020-01-01T00:00:00Z","verdict":"reload_ok"}}"#
        )
        .unwrap();
        let result = poll_reload_status(path.to_str().unwrap(), t0, 10);
        assert_eq!(result, None);
        cleanup(&dir);
    }
}
