//! `client_tokens` SQLite schema + CRUD.
//!
//! This is the persistent side of the client-token system. Tokens are
//! stored as `(prefix, sha256(wire), user_id, label, expiry, revocation)`
//! rows. Only the hash and prefix live here — the full wire form exists
//! only in the brief window between [`super::Token::generate`] and the
//! single time it is shown to the operator.

use rusqlite::{params, Connection};

#[cfg(test)]
mod tests;

/// A live token row joined with whatever the proxy needs to authorize a
/// request. The struct intentionally does NOT carry the wire form or the
/// raw hash by default — those stay inside the store layer.
#[derive(Debug, Clone)]
pub struct ClientTokenRow {
    pub id: i64,
    pub prefix: String,
    pub user_id: i64,
    pub label: Option<String>,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub revoked_at: Option<String>,
    pub last_used_at: Option<String>,
}

/// Apply the `client_tokens` schema to an open SQLite connection.
///
/// Idempotent; safe to call on every startup. Uses `IF NOT EXISTS` so it
/// coexists with the existing budget tables on the same file.
pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS client_tokens (
            id           INTEGER PRIMARY KEY,
            prefix       TEXT NOT NULL UNIQUE,
            hash         BLOB NOT NULL,
            user_id      INTEGER NOT NULL,
            label        TEXT,
            created_at   TEXT NOT NULL,
            expires_at   TEXT,
            revoked_at   TEXT,
            last_used_at TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_client_tokens_prefix
            ON client_tokens(prefix);
        CREATE INDEX IF NOT EXISTS idx_client_tokens_user
            ON client_tokens(user_id);
        "#,
    )
}

/// Insert a freshly-generated token. Returns the row id.
///
/// `user_id` is a forward reference to the `users` table that
/// `user-management.md` introduces; today the caller passes a bare
/// integer (`0` for a single-tenant install is fine) and the FK is
/// non-enforcing.
pub fn insert(
    conn: &Connection,
    prefix: &str,
    hash: &[u8; 32],
    user_id: i64,
    label: Option<&str>,
    expires_at: Option<&str>,
) -> rusqlite::Result<i64> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO client_tokens
            (prefix, hash, user_id, label, created_at, expires_at)
         VALUES (?, ?, ?, ?, ?, ?)",
        params![prefix, hash.as_slice(), user_id, label, now, expires_at],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Look up a row by prefix.
///
/// Returns the stored hash separately so the caller (the verification
/// path) can do the constant-time comparison without the hash leaking
/// into a `Debug` or `Clone`.
pub fn lookup_by_prefix(
    conn: &Connection,
    prefix: &str,
) -> rusqlite::Result<Option<(ClientTokenRow, [u8; 32])>> {
    let mut stmt = conn.prepare(
        "SELECT id, prefix, hash, user_id, label,
                created_at, expires_at, revoked_at, last_used_at
         FROM client_tokens
         WHERE prefix = ?",
    )?;
    let mut rows = stmt.query(params![prefix])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };

    let id: i64 = row.get(0)?;
    let prefix: String = row.get(1)?;
    let hash_blob: Vec<u8> = row.get(2)?;
    let user_id: i64 = row.get(3)?;
    let label: Option<String> = row.get(4)?;
    let created_at: String = row.get(5)?;
    let expires_at: Option<String> = row.get(6)?;
    let revoked_at: Option<String> = row.get(7)?;
    let last_used_at: Option<String> = row.get(8)?;

    if hash_blob.len() != 32 {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Blob,
            "client_tokens.hash is not 32 bytes".into(),
        ));
    }
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&hash_blob);

    Ok(Some((
        ClientTokenRow {
            id,
            prefix,
            user_id,
            label,
            created_at,
            expires_at,
            revoked_at,
            last_used_at,
        },
        hash,
    )))
}

/// List all tokens for a user. Prefix only — no hash exposed.
pub fn list_for_user(conn: &Connection, user_id: i64) -> rusqlite::Result<Vec<ClientTokenRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, prefix, user_id, label, created_at, expires_at, revoked_at, last_used_at
         FROM client_tokens
         WHERE user_id = ?
         ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map(params![user_id], |row| {
        Ok(ClientTokenRow {
            id: row.get(0)?,
            prefix: row.get(1)?,
            user_id: row.get(2)?,
            label: row.get(3)?,
            created_at: row.get(4)?,
            expires_at: row.get(5)?,
            revoked_at: row.get(6)?,
            last_used_at: row.get(7)?,
        })
    })?;
    rows.collect()
}

/// Mark a token revoked. Idempotent — calling revoke on an already-revoked
/// token leaves the original `revoked_at` in place.
pub fn revoke(conn: &Connection, id: i64) -> rusqlite::Result<usize> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE client_tokens
         SET revoked_at = ?
         WHERE id = ? AND revoked_at IS NULL",
        params![now, id],
    )
}
