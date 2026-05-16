//! Runtime handle for the client-auth subsystem.
//!
//! Owns the SQLite connection that backs the `client_tokens` table and (in
//! a later commit) the in-memory verification cache. Threaded through
//! `RuntimeHandles` so hot reload preserves it across SIGHUP.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::config::AuthConfig;

/// Shared handle to the client-auth backing store + cache.
///
/// Cloning is cheap (`Arc` wraps). The struct is intentionally small so
/// adding it to `RuntimeHandles` (already `Clone`) costs nothing.
#[derive(Clone)]
pub struct ClientAuth {
    inner: Arc<Inner>,
}

struct Inner {
    /// Dedicated SQLite connection for the `client_tokens` table.
    ///
    /// Distinct from `SqliteBudgetStore`'s connection (both point at the
    /// same file when configured that way; SQLite multi-handle access is
    /// safe through its file lock). Kept separate so the budget code
    /// does not have to learn about client-auth, and vice versa.
    conn: Mutex<Connection>,
    config: AuthConfig,
}

impl ClientAuth {
    /// Open the backing store and apply the `client_tokens` migration.
    ///
    /// `db_path` should match the budget DB path so a single SQLite file
    /// holds both — operators get one place to back up, one place to
    /// configure permissions, one place to inspect with `sqlite3`.
    pub fn open(db_path: &str, config: AuthConfig) -> Result<Self> {
        let conn = Connection::open(db_path)
            .with_context(|| format!("opening client_tokens DB at {db_path}"))?;
        super::store::migrate(&conn).context("applying client_tokens migration")?;
        Ok(Self {
            inner: Arc::new(Inner {
                conn: Mutex::new(conn),
                config,
            }),
        })
    }

    /// Returns true when the verification middleware should run. False
    /// short-circuits to "allow" so unauthenticated callers pass through
    /// during Stage 1 rollout (per docs/design/client-auth.md).
    pub fn enabled(&self) -> bool {
        self.inner.config.enabled
    }

    pub fn env_marker(&self) -> char {
        self.inner.config.env_marker
    }

    /// When true, the verification middleware rejects non-loopback
    /// requests that arrive over plain HTTP (lack `X-Forwarded-Proto:
    /// https` from the trusted reverse proxy).
    pub fn require_https(&self) -> bool {
        self.inner.config.require_https
    }

    /// Acquire the SQLite connection. Held under a Mutex; callers should
    /// release it quickly. Used by the verification path and by admin
    /// endpoints for create/list/revoke.
    pub fn with_conn<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Connection) -> R,
    {
        let conn = self.inner.conn.lock().expect("client_auth conn poisoned");
        f(&conn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_auth::{store, Token};

    /// Build a `ClientAuth` against `:memory:` — sufficient for unit-level
    /// behavior. The in-memory DB is destroyed when the connection drops,
    /// which means every test gets a fresh one.
    fn in_memory(cfg: AuthConfig) -> ClientAuth {
        ClientAuth::open(":memory:", cfg).expect("open in-memory")
    }

    #[test]
    fn open_creates_the_table_and_reports_disabled_by_default() {
        let ca = in_memory(AuthConfig::default());
        assert!(!ca.enabled());
        assert_eq!(ca.env_marker(), 'p');
    }

    #[test]
    fn with_conn_can_read_and_write_via_store() {
        let ca = in_memory(AuthConfig::default());

        let token = Token::generate('p');
        ca.with_conn(|c| {
            store::insert(c, &token.prefix, &token.hash, 42, Some("test"), None).unwrap();
        });

        let (row, hash) = ca
            .with_conn(|c| store::lookup_by_prefix(c, &token.prefix))
            .unwrap()
            .expect("row");
        assert_eq!(row.user_id, 42);
        assert_eq!(row.label.as_deref(), Some("test"));
        assert_eq!(hash, token.hash);
    }

    #[test]
    fn config_values_are_exposed() {
        let cfg = AuthConfig {
            enabled: true,
            env_marker: 't',
            cache_capacity: 4096,
            cache_ttl_secs: 30,
            require_https: true,
        };
        let ca = in_memory(cfg);
        assert!(ca.enabled());
        assert_eq!(ca.env_marker(), 't');
    }
}
