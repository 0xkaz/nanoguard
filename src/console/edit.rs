//! File editing contract for the nanoguard console.
//!
//! Every write is atomic (temp + fsync + rename), validated with the same
//! parsers the proxy uses, and backed up before the rename.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Maximum backups per file (default).
const DEFAULT_BACKUP_LIMIT: usize = 20;

/// The result of a validation attempt.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ValidationResult {
    pub valid: bool,
    pub error: Option<String>,
}

/// Validate a dict file (matcher format) without writing.
pub fn validate_dict(content: &str) -> ValidationResult {
    match validate_dict_inner(content) {
        Ok(()) => ValidationResult {
            valid: true,
            error: None,
        },
        Err(e) => ValidationResult {
            valid: false,
            error: Some(format!("{e:#}")),
        },
    }
}

fn validate_dict_inner(content: &str) -> Result<()> {
    for (idx, raw_line) in content.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut cols = line.split('\t').map(str::trim);
        let Some(pattern) = cols.next().filter(|s| !s.is_empty()) else {
            continue;
        };
        let key = cols
            .next()
            .ok_or_else(|| anyhow::anyhow!("line {}: missing key", idx + 1))?;
        let key = key
            .parse::<u32>()
            .with_context(|| format!("line {}: invalid key `{}`", idx + 1, key))?;
        match key {
            0..=2 => {}
            other => bail!("line {}: unsupported key `{}`", idx + 1, other),
        }
        // If it looks like a regex, try to compile it.
        if let Some(body) = pattern.strip_prefix('/').and_then(|s| s.strip_suffix('/')) {
            if !body.is_empty() {
                regex::RegexBuilder::new(body)
                    .case_insensitive(true)
                    .build()
                    .with_context(|| format!("line {}: invalid regex `{}`", idx + 1, body))?;
            }
        }
    }
    Ok(())
}

/// Validate a policy YAML bundle without writing.
pub fn validate_policy(content: &str) -> ValidationResult {
    match crate::policy::Policy::from_yaml_str(content) {
        Ok(_) => ValidationResult {
            valid: true,
            error: None,
        },
        Err(e) => ValidationResult {
            valid: false,
            error: Some(format!("{e:#}")),
        },
    }
}

/// Validate nanoguard.toml content.
pub fn validate_toml(content: &str) -> ValidationResult {
    match crate::config::Config::from_file_content(content) {
        Ok(_) => ValidationResult {
            valid: true,
            error: None,
        },
        Err(e) => ValidationResult {
            valid: false,
            error: Some(format!("{e:#}")),
        },
    }
}

/// Validate a PII regex dict file (redactor format) without writing.
pub fn validate_pii_dict(content: &str) -> ValidationResult {
    match validate_pii_dict_inner(content) {
        Ok(()) => ValidationResult {
            valid: true,
            error: None,
        },
        Err(e) => ValidationResult {
            valid: false,
            error: Some(format!("{e:#}")),
        },
    }
}

fn validate_pii_dict_inner(content: &str) -> Result<()> {
    for (idx, raw_line) in content.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut cols = line.split('\t');
        let Some(pattern_field) = cols.next() else {
            continue;
        };
        let Some(stripped) = pattern_field
            .strip_prefix('/')
            .and_then(|s| s.rsplit_once('/'))
            .map(|(p, _)| p)
        else {
            continue;
        };
        if stripped.is_empty() {
            continue;
        }
        regex::RegexBuilder::new(stripped)
            .build()
            .with_context(|| format!("line {}: invalid regex `{}`", idx + 1, stripped))?;
    }
    Ok(())
}

/// Validate content based on inferred file type from path.
pub fn validate_by_path(path: &str, content: &str) -> ValidationResult {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".yaml") || lower.ends_with(".yml") {
        validate_policy(content)
    } else if lower.ends_with(".toml") {
        validate_toml(content)
    } else if lower.contains("pii") && lower.ends_with(".txt") {
        validate_pii_dict(content)
    } else if lower.ends_with(".txt") {
        validate_dict(content)
    } else {
        ValidationResult {
            valid: false,
            error: Some(format!("unknown file type for `{}`", path)),
        }
    }
}

/// Compute a SHA-256 hex hash of the content.
pub fn hash_content(content: &str) -> String {
    let mut h = Sha256::new();
    h.update(content.as_bytes());
    format!("{:x}", h.finalize())
}

/// Write `content` to `path` atomically with backup and validation.
///
/// Returns `(before_hash, after_hash)` on success.
pub fn atomic_write(
    path: impl AsRef<Path>,
    content: &str,
    validator: impl FnOnce(&str) -> ValidationResult,
) -> Result<(String, String)> {
    let path = path.as_ref();
    let before = std::fs::read_to_string(path).unwrap_or_default();
    let before_hash = hash_content(&before);

    let validation = validator(content);
    if !validation.valid {
        bail!(
            "validation failed: {}",
            validation.error.unwrap_or_default()
        );
    }

    // Ensure parent directory exists.
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    // Backup before writing.
    if !before.is_empty() {
        backup_file(path, &before)?;
    }

    // Atomic write: temp file in same directory, fsync, rename.
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let dir = dir.unwrap_or_else(|| Path::new("."));
    let nonce = rand::random::<u64>();
    let tmp_name = format!(
        ".{}.tmp.{}.{}.{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        nonce,
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    );
    let tmp_path = dir.join(tmp_name);

    std::fs::write(&tmp_path, content).with_context(|| format!("writing temp file {:?}", tmp_path))?;

    // fsync the temp file.
    {
        let file = std::fs::File::open(&tmp_path)
            .with_context(|| format!("opening temp file {:?} for fsync", tmp_path))?;
        file.sync_all()
            .with_context(|| format!("fsync temp file {:?}", tmp_path))?;
    }

    // fsync the directory so the rename is durable.
    #[cfg(unix)]
    {
        let dir_file = std::fs::File::open(dir)
            .with_context(|| format!("opening directory {:?} for fsync", dir))?;
        let _ = dir_file.sync_all();
    }

    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("renaming {:?} to {:?}", tmp_path, path))?;

    let after_hash = hash_content(content);
    Ok((before_hash, after_hash))
}

/// Copy the current content of `path` into `.nanoguard-backups/`.
fn backup_file(path: &Path, content: &str) -> Result<()> {
    let backup_dir = if let Some(parent) = path.parent() {
        if parent.as_os_str().is_empty() {
            PathBuf::from(".nanoguard-backups")
        } else {
            parent.join(".nanoguard-backups")
        }
    } else {
        PathBuf::from(".nanoguard-backups")
    };
    std::fs::create_dir_all(&backup_dir)?;

    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = path.extension().unwrap_or_default().to_string_lossy();
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S%f");
    let backup_name = if ext.is_empty() {
        format!("{}.bak.{}", stem, ts)
    } else {
        format!("{}.bak.{}.{}", stem, ts, ext)
    };
    let backup_path = backup_dir.join(&backup_name);
    std::fs::write(&backup_path, content)
        .with_context(|| format!("writing backup {:?}", backup_path))?;

    // Prune old backups.
    prune_backups(&backup_dir, &stem, DEFAULT_BACKUP_LIMIT)?;

    Ok(())
}

fn prune_backups(dir: &Path, stem: &str, limit: usize) -> Result<()> {
    let mut entries: Vec<(PathBuf, std::time::SystemTime)> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with(&format!("{}.", stem)) && name.contains(".bak.")
        })
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            let modified = meta.modified().ok()?;
            Some((e.path(), modified))
        })
        .collect();

    entries.sort_by(|a, b| b.1.cmp(&a.1)); // newest first

    for (path, _) in entries.iter().skip(limit) {
        let _ = std::fs::remove_file(path);
    }

    Ok(())
}

/// List available backups for a file.
pub fn list_backups(path: impl AsRef<Path>) -> Result<Vec<BackupInfo>> {
    let path = path.as_ref();
    let backup_dir = if let Some(parent) = path.parent() {
        if parent.as_os_str().is_empty() {
            PathBuf::from(".nanoguard-backups")
        } else {
            parent.join(".nanoguard-backups")
        }
    } else {
        PathBuf::from(".nanoguard-backups")
    };

    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let mut entries: Vec<BackupInfo> = Vec::new();

    if let Ok(dir) = std::fs::read_dir(&backup_dir) {
        for e in dir.filter_map(|e| e.ok()) {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with(&format!("{}.", stem)) || !name.contains(".bak.") {
                continue;
            }
            let meta = match e.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let modified = match meta.modified() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let secs = modified
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            entries.push(BackupInfo {
                name: name.to_string(),
                path: e.path().to_string_lossy().to_string(),
                size: meta.len(),
                created_at: secs,
            });
        }
    }

    entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(entries)
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BackupInfo {
    pub name: String,
    pub path: String,
    pub size: u64,
    pub created_at: i64,
}

/// Revert a file to a named backup.
pub fn revert_to_backup(file_path: impl AsRef<Path>, backup_name: &str) -> Result<String> {
    let file_path = file_path.as_ref();
    let backup_dir = if let Some(parent) = file_path.parent() {
        if parent.as_os_str().is_empty() {
            PathBuf::from(".nanoguard-backups")
        } else {
            parent.join(".nanoguard-backups")
        }
    } else {
        PathBuf::from(".nanoguard-backups")
    };

    let backup_path = backup_dir.join(backup_name);
    if !backup_path.exists() {
        bail!("backup not found: {}", backup_name);
    }

    let content = std::fs::read_to_string(&backup_path)
        .with_context(|| format!("reading backup {:?}", backup_path))?;

    // Write back atomically (no backup of the revert itself to avoid loops).
    let dir = file_path.parent().filter(|p| !p.as_os_str().is_empty());
    let dir = dir.unwrap_or_else(|| Path::new("."));
    let nonce = rand::random::<u64>();
    let tmp_name = format!(
        ".{}.tmp.{}.{}.{}",
        file_path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        nonce,
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    );
    let tmp_path = dir.join(tmp_name);

    std::fs::write(&tmp_path, &content)?;
    std::fs::rename(&tmp_path, file_path)?;

    Ok(content)
}

/// Read the content of a backup file.
pub fn read_backup(backup_path: impl AsRef<Path>) -> Result<String> {
    let path = backup_path.as_ref();
    std::fs::read_to_string(path).with_context(|| format!("reading backup {:?}", path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nanoguard-edit-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn atomic_write_creates_file() {
        let dir = tmp_dir();
        let path = dir.join("test.txt");
        let content = "hello world\t0\t1.0\n";
        let (_, after) = atomic_write(&path, content, validate_dict).unwrap();
        assert_eq!(after, hash_content(content));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        cleanup(&dir);
    }

    #[test]
    fn atomic_write_rejects_invalid_dict() {
        let dir = tmp_dir();
        let path = dir.join("test.txt");
        let result = atomic_write(&path, "badline\t99", validate_dict);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unsupported key"));
        cleanup(&dir);
    }

    #[test]
    fn atomic_write_rejects_invalid_regex_in_dict() {
        let dir = tmp_dir();
        let path = dir.join("test.txt");
        let result = atomic_write(&path, "/[invalid(/\t0", validate_dict);
        assert!(result.is_err());
        cleanup(&dir);
    }

    #[test]
    fn atomic_write_creates_backup() {
        let dir = tmp_dir();
        let path = dir.join("test.txt");
        std::fs::write(&path, "original\t0\t1.0\n").unwrap();
        let _ = atomic_write(&path, "updated\t0\t1.0\n", validate_dict).unwrap();
        let backups = list_backups(&path).unwrap();
        assert!(!backups.is_empty());
        assert!(backups[0].name.starts_with("test.bak."));
        cleanup(&dir);
    }

    #[test]
    fn revert_restores_backup() {
        let dir = tmp_dir();
        let path = dir.join("test.txt");
        std::fs::write(&path, "original\t0\t1.0\n").unwrap();
        let _ = atomic_write(&path, "updated\t0\t1.0\n", validate_dict).unwrap();
        let backups = list_backups(&path).unwrap();
        let backup_name = &backups[0].name;
        let content = revert_to_backup(&path, backup_name).unwrap();
        assert_eq!(content, "original\t0\t1.0\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original\t0\t1.0\n");
        cleanup(&dir);
    }

    #[test]
    fn validate_policy_catches_bad_yaml() {
        let r = validate_policy("not: valid: yaml: [");
        assert!(!r.valid);
    }

    #[test]
    fn validate_dict_accepts_valid() {
        let r = validate_dict("ignore previous instructions\t0\t10.0\n");
        assert!(r.valid);
    }

    #[test]
    fn validate_pii_dict_accepts_valid_regex() {
        let r = validate_pii_dict("/[A-Za-z0-9]+/\tEMAIL\n");
        assert!(r.valid);
    }

    #[test]
    fn validate_pii_dict_rejects_invalid_regex() {
        let r = validate_pii_dict("/[invalid(/\tEMAIL\n");
        assert!(!r.valid);
    }
}
