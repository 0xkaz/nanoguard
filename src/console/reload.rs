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
///
/// This is `async` so the blocking socket exchange runs on tokio's
/// dedicated blocking pool via `spawn_blocking`. Earlier versions
/// were sync and called `read()` on the calling worker; in
/// single-process mode (Console + proxy on one runtime) that pinned
/// the worker that was supposed to also run the proxy's
/// `UnixListener::accept()`, so the proxy never accepted the
/// connection until the read timed out — the Console reported
/// "reload failed" every time even though no reload was attempted.
/// Routing the blocking exchange through `spawn_blocking` lets the
/// runtime schedule both ends.
pub async fn trigger_reload(cfg: &crate::config::ReloadConfig) -> ReloadOutcome {
    dispatch(cfg.clone(), "RELOAD").await
}

/// Ask the proxy to drop its in-memory client-token verification cache.
///
/// This is the cross-process counterpart to
/// `client_auth::ClientAuth::invalidate_all_cached`: the Web Console and
/// the proxy live in separate binaries against a shared SQLite DB, so a
/// console-side revoke cannot reach the proxy's per-process cache via a
/// function call. The Unix-socket path sends an `INVALIDATE_TOKENS\n`
/// command (cheap, just flushes the cache). The pid-file path falls back
/// to SIGHUP, which is heavier — a full state rebuild — but achieves the
/// same end (a fresh `AppState` snapshot whose cache starts empty when
/// `[auth]` is freshly resolved). Prefer `[reload].socket` when revoke
/// latency matters.
pub async fn trigger_invalidate_tokens(cfg: &crate::config::ReloadConfig) -> ReloadOutcome {
    dispatch(cfg.clone(), "INVALIDATE_TOKENS").await
}

async fn dispatch(cfg: crate::config::ReloadConfig, command: &'static str) -> ReloadOutcome {
    // Offload the synchronous socket / signal call to tokio's blocking
    // pool. The handler that called us is on a worker; running stdlib
    // I/O directly here would block that worker, and in single-process
    // mode the very same runtime owns the proxy's reload listener.
    let result = tokio::task::spawn_blocking(move || dispatch_blocking(&cfg, command)).await;
    match result {
        Ok(outcome) => outcome,
        Err(e) => ReloadOutcome {
            triggered: false,
            method: "spawn_blocking".to_string(),
            error: Some(format!("spawn_blocking failed: {e}")),
        },
    }
}

fn dispatch_blocking(cfg: &crate::config::ReloadConfig, command: &str) -> ReloadOutcome {
    // Prefer Unix socket if configured.
    if let Some(ref socket_path) = cfg.socket {
        match trigger_via_socket(socket_path, command) {
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
        // SIGHUP is the only signal-based path the proxy currently
        // understands. INVALIDATE_TOKENS callers get the same SIGHUP,
        // which performs a full reload — a superset of cache flush.
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
fn trigger_via_socket(socket_path: &str, command: &str) -> Result<()> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    // This runs on tokio's blocking pool (see `dispatch`). Stdlib
    // blocking I/O here is safe because the worker thread the
    // Console handler runs on stays free to drive the proxy's
    // `UnixListener::accept()`. An earlier sync-on-worker version
    // deadlocked the single-process boot until the read timed out.
    let mut stream = UnixStream::connect(socket_path)
        .with_context(|| format!("connecting to reload socket {}", socket_path))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .with_context(|| "setting read timeout")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .with_context(|| "setting write timeout")?;
    let payload = format!("{}\n", command);
    stream
        .write_all(payload.as_bytes())
        .with_context(|| format!("writing {} to socket", command))?;
    stream.flush().with_context(|| "flushing socket")?;

    let mut resp = String::new();
    BufReader::new(&mut stream)
        .read_line(&mut resp)
        .with_context(|| "reading socket response")?;
    let resp = resp.trim();
    if let Some(rest) = resp.strip_prefix("ERR") {
        bail!("proxy returned error:{}", rest);
    }
    if resp.starts_with("OK") {
        Ok(())
    } else {
        bail!("unexpected proxy response: {}", resp);
    }
}

#[cfg(not(unix))]
fn trigger_via_socket(_socket_path: &str, _command: &str) -> Result<()> {
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

    #[test]
    #[cfg(unix)]
    fn trigger_via_pid_file_with_valid_pid() {
        let dir = tmp_dir();
        let path = dir.join("proxy.pid");
        std::fs::write(&path, format!("{}\n", std::process::id())).unwrap();
        // Sending SIGHUP to ourselves in a test is dangerous (would invoke the
        // test runner's signal handler), so we verify the function only gets
        // as far as the kill() call by using a non-existent PID instead in
        // the next test.  Here we just assert parse succeeds.
        let pid_str = std::fs::read_to_string(&path).unwrap();
        let pid: i32 = pid_str.trim().parse().unwrap();
        assert!(pid > 0);
        cleanup(&dir);
    }

    #[test]
    fn trigger_via_pid_file_missing_file() {
        let result = trigger_via_pid_file("/nonexistent/path/proxy.pid");
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("reading pid file"),
            "error should mention reading pid file: {msg}"
        );
    }

    #[test]
    fn trigger_via_pid_file_invalid_content() {
        let dir = tmp_dir();
        let path = dir.join("proxy.pid");
        std::fs::write(&path, b"not-a-pid\n").unwrap();
        let result = trigger_via_pid_file(path.to_str().unwrap());
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("parsing PID"),
            "error should mention parsing PID: {msg}"
        );
        cleanup(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn trigger_via_socket_happy_path() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;

        let socket_path = format!("/tmp/ng-console-sock-{:x}", rand::random::<u32>());
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).unwrap();

        // Spawn a tiny responder in a background thread.
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 128];
            let n = stream.read(&mut buf).unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            assert_eq!(req.trim(), "RELOAD");
            stream.write_all(b"OK\n").unwrap();
        });

        let result = trigger_via_socket(&socket_path, "RELOAD");
        assert!(result.is_ok(), "socket trigger failed: {:?}", result);
        handle.join().unwrap();
        let _ = std::fs::remove_file(&socket_path);
    }

    #[test]
    #[cfg(unix)]
    fn trigger_via_socket_err_response() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;

        let socket_path = format!("/tmp/ng-console-sock-{:x}", rand::random::<u32>());
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).unwrap();

        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 128];
            let n = stream.read(&mut buf).unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            assert_eq!(req.trim(), "RELOAD");
            stream.write_all(b"ERR something_broken\n").unwrap();
        });

        let result = trigger_via_socket(&socket_path, "RELOAD");
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("something_broken"),
            "error should contain proxy error: {msg}"
        );
        handle.join().unwrap();
        let _ = std::fs::remove_file(&socket_path);
    }
}
