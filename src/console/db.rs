//! Database layer for the nanoguard console.
//!
//! Manages the `users` and `user_sessions` tables alongside the existing
//! `client_tokens` and budget tables in the shared SQLite file.

use rusqlite::{params, Connection, OptionalExtension};
use std::sync::Mutex;

/// Console database handle. Wraps the SQLite connection in a mutex because
/// `rusqlite::Connection` is `Send` but not `Sync`.
pub struct ConsoleDb {
    conn: Mutex<Connection>,
}

impl ConsoleDb {
    /// Open the database at `path`, creating tables if necessary.
    pub fn open(path: &str) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn with_conn<F, T>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(&Connection) -> anyhow::Result<T>,
    {
        let conn = self.conn.lock().unwrap();
        f(&conn)
    }
}

fn migrate(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS users (
            id              INTEGER PRIMARY KEY,
            username        TEXT NOT NULL UNIQUE,
            display_name    TEXT,
            email           TEXT,
            role            TEXT NOT NULL DEFAULT 'user',
            service_account INTEGER NOT NULL DEFAULT 0,
            disabled        INTEGER NOT NULL DEFAULT 0,
            allowed_models  TEXT NOT NULL DEFAULT '[]',
            budget_limit    INTEGER,
            oidc_sub        TEXT UNIQUE,
            password_hash   BLOB,
            created_at      TEXT NOT NULL,
            last_login_at   TEXT
        );

        CREATE TABLE IF NOT EXISTS user_sessions (
            id           BLOB PRIMARY KEY,
            user_id      INTEGER NOT NULL,
            created_at   TEXT NOT NULL,
            expires_at   TEXT NOT NULL,
            last_seen_at TEXT NOT NULL,
            user_agent   TEXT,
            ip           TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_user_sessions_user ON user_sessions(user_id);
        CREATE INDEX IF NOT EXISTS idx_user_sessions_expires ON user_sessions(expires_at);
        "#,
    )?;
    Ok(())
}

// ── User CRUD ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub role: String,
    pub service_account: bool,
    pub disabled: bool,
    pub allowed_models: String,
    pub budget_limit: Option<i64>,
    pub oidc_sub: Option<String>,
    pub password_hash: Option<Vec<u8>>,
    pub created_at: String,
    pub last_login_at: Option<String>,
}

pub fn user_by_id(conn: &Connection, id: i64) -> anyhow::Result<Option<User>> {
    let mut stmt = conn.prepare(
        "SELECT id, username, display_name, email, role, service_account,
                disabled, allowed_models, budget_limit, oidc_sub, password_hash,
                created_at, last_login_at
         FROM users WHERE id = ?",
    )?;
    row_to_user(stmt.query_row(params![id], map_user).optional()?)
}

pub fn user_by_username(conn: &Connection, username: &str) -> anyhow::Result<Option<User>> {
    let mut stmt = conn.prepare(
        "SELECT id, username, display_name, email, role, service_account,
                disabled, allowed_models, budget_limit, oidc_sub, password_hash,
                created_at, last_login_at
         FROM users WHERE username = ?",
    )?;
    row_to_user(stmt.query_row(params![username], map_user).optional()?)
}

fn map_user(row: &rusqlite::Row) -> rusqlite::Result<User> {
    Ok(User {
        id: row.get(0)?,
        username: row.get(1)?,
        display_name: row.get(2)?,
        email: row.get(3)?,
        role: row.get(4)?,
        service_account: row.get::<_, i64>(5)? != 0,
        disabled: row.get::<_, i64>(6)? != 0,
        allowed_models: row.get(7)?,
        budget_limit: row.get(8)?,
        oidc_sub: row.get(9)?,
        password_hash: row.get(10)?,
        created_at: row.get(11)?,
        last_login_at: row.get(12)?,
    })
}

fn row_to_user(row: Option<User>) -> anyhow::Result<Option<User>> {
    Ok(row)
}

pub fn insert_user(
    conn: &Connection,
    username: &str,
    display_name: Option<&str>,
    email: Option<&str>,
    role: &str,
    password_hash: Option<&[u8]>,
) -> anyhow::Result<i64> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO users (username, display_name, email, role, password_hash, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
        params![username, display_name, email, role, password_hash, now],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn update_last_login(conn: &Connection, id: i64) -> anyhow::Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE users SET last_login_at = ? WHERE id = ?",
        params![now, id],
    )?;
    Ok(())
}

pub fn list_users(conn: &Connection) -> anyhow::Result<Vec<User>> {
    let mut stmt = conn.prepare(
        "SELECT id, username, display_name, email, role, service_account,
                disabled, allowed_models, budget_limit, oidc_sub, password_hash,
                created_at, last_login_at
         FROM users ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map([], map_user)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

pub fn update_user(
    conn: &Connection,
    id: i64,
    display_name: Option<&str>,
    email: Option<&str>,
    role: Option<&str>,
    disabled: Option<bool>,
    allowed_models: Option<&str>,
    budget_limit: Option<Option<i64>>,
) -> anyhow::Result<usize> {
    let mut total = 0;
    if let Some(v) = display_name {
        total += conn.execute(
            "UPDATE users SET display_name = ?1 WHERE id = ?2",
            params![v, id],
        )?;
    }
    if let Some(v) = email {
        total += conn.execute(
            "UPDATE users SET email = ?1 WHERE id = ?2",
            params![v, id],
        )?;
    }
    if let Some(v) = role {
        total += conn.execute(
            "UPDATE users SET role = ?1 WHERE id = ?2",
            params![v, id],
        )?;
    }
    if let Some(v) = disabled {
        let flag: i64 = if v { 1 } else { 0 };
        total += conn.execute(
            "UPDATE users SET disabled = ?1 WHERE id = ?2",
            params![flag, id],
        )?;
    }
    if let Some(v) = allowed_models {
        total += conn.execute(
            "UPDATE users SET allowed_models = ?1 WHERE id = ?2",
            params![v, id],
        )?;
    }
    if let Some(v) = budget_limit {
        total += conn.execute(
            "UPDATE users SET budget_limit = ?1 WHERE id = ?2",
            params![v, id],
        )?;
    }
    Ok(total)
}

pub fn set_password_hash(conn: &Connection, id: i64, hash: &[u8]) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE users SET password_hash = ? WHERE id = ?",
        params![hash, id],
    )?;
    Ok(())
}

// ── Session CRUD ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Session {
    pub id: Vec<u8>,
    pub user_id: i64,
    pub created_at: String,
    pub expires_at: String,
    pub last_seen_at: String,
    pub user_agent: Option<String>,
    pub ip: Option<String>,
}

pub fn create_session(
    conn: &Connection,
    id: &[u8],
    user_id: i64,
    expires_at: &str,
    user_agent: Option<&str>,
    ip: Option<&str>,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO user_sessions (id, user_id, created_at, expires_at, last_seen_at, user_agent, ip)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        params![id, user_id, now, expires_at, now, user_agent, ip],
    )?;
    Ok(())
}

pub fn touch_session(conn: &Connection, id: &[u8]) -> anyhow::Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE user_sessions SET last_seen_at = ? WHERE id = ?",
        params![now, id],
    )?;
    Ok(())
}

pub fn session_by_id(conn: &Connection, id: &[u8]) -> anyhow::Result<Option<Session>> {
    let mut stmt = conn.prepare(
        "SELECT id, user_id, created_at, expires_at, last_seen_at, user_agent, ip
         FROM user_sessions WHERE id = ?",
    )?;
    let row = stmt
        .query_row(params![id], |row| {
            Ok(Session {
                id: row.get(0)?,
                user_id: row.get(1)?,
                created_at: row.get(2)?,
                expires_at: row.get(3)?,
                last_seen_at: row.get(4)?,
                user_agent: row.get(5)?,
                ip: row.get(6)?,
            })
        })
        .optional()?;
    Ok(row)
}

pub fn delete_session(conn: &Connection, id: &[u8]) -> anyhow::Result<()> {
    conn.execute("DELETE FROM user_sessions WHERE id = ?", params![id])?;
    Ok(())
}

pub fn delete_user_sessions(conn: &Connection, user_id: i64) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM user_sessions WHERE user_id = ?",
        params![user_id],
    )?;
    Ok(())
}

pub fn prune_expired_sessions(conn: &Connection) -> anyhow::Result<usize> {
    let now = chrono::Utc::now().to_rfc3339();
    let n = conn.execute(
        "DELETE FROM user_sessions WHERE expires_at < ?",
        params![now],
    )?;
    Ok(n)
}

pub fn list_sessions_for_user(conn: &Connection, user_id: i64) -> anyhow::Result<Vec<Session>> {
    let mut stmt = conn.prepare(
        "SELECT id, user_id, created_at, expires_at, last_seen_at, user_agent, ip
         FROM user_sessions WHERE user_id = ? ORDER BY last_seen_at DESC",
    )?;
    let rows = stmt.query_map(params![user_id], |row| {
        Ok(Session {
            id: row.get(0)?,
            user_id: row.get(1)?,
            created_at: row.get(2)?,
            expires_at: row.get(3)?,
            last_seen_at: row.get(4)?,
            user_agent: row.get(5)?,
            ip: row.get(6)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

// ── Bootstrap ───────────────────────────────────────────────────────────────

/// If no users exist and a bootstrap admin is configured, create it.
pub fn maybe_bootstrap_admin(
    conn: &Connection,
    username: &str,
    password_hash: &[u8],
) -> anyhow::Result<bool> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
    if count > 0 {
        return Ok(false);
    }
    insert_user(conn, username, Some(username), None, "admin", Some(password_hash))?;
    Ok(true)
}


#[cfg(test)]
mod tests;
