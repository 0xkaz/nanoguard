//! Runtime handle for the client-auth subsystem.
//!
//! Owns the SQLite connection that backs the `client_tokens` table and
//! the in-memory verification cache. Threaded through `RuntimeHandles`
//! so hot reload preserves it across SIGHUP.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::client_auth::cache::{CachedToken, TokenCache};
use crate::client_auth::store;
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
    /// In-memory verification cache. Lookups go through this before
    /// touching SQLite; admin revoke calls `invalidate` to evict the
    /// affected prefix immediately.
    cache: TokenCache,
    config: AuthConfig,
}

impl ClientAuth {
    /// Open the backing store and apply the `client_tokens` migration.
    ///
    /// `db_path` should match the budget DB path so a single SQLite file
    /// holds both — operators get one place to back up, one place to
    /// configure permissions, one place to inspect with `sqlite3`.
    pub fn open(db_path: &str, config: AuthConfig) -> Result<Self> {
        // Validate `env_marker` at startup. The wire token shape is
        // "ng_<env>_<24 char secret>" and `PrefixedToken::parse`
        // requires the env-marker position to be ASCII alphanumeric.
        // Without this check, an operator who sets, say,
        // `env_marker = "@"` would see tokens mint successfully but
        // every subsequent request would 401 with no indication that
        // the configuration is wrong. Fail fast so the misconfig
        // surfaces at startup, not at first request.
        if !config.env_marker.is_ascii_alphanumeric() {
            anyhow::bail!(
                "[auth].env_marker must be a single ASCII alphanumeric character (got {:?})",
                config.env_marker
            );
        }

        let conn = Connection::open(db_path)
            .with_context(|| format!("opening client_tokens DB at {db_path}"))?;
        super::store::migrate(&conn).context("applying client_tokens migration")?;
        let cache = TokenCache::new(
            config.cache_capacity,
            Duration::from_secs(config.cache_ttl_secs),
        );
        Ok(Self {
            inner: Arc::new(Inner {
                conn: Mutex::new(conn),
                cache,
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

    /// Look up a token by its 10-char prefix. Hits the in-memory cache
    /// first; on miss reads SQLite and populates the cache.
    ///
    /// Returns the cached `(row, hash)` pair so the caller can do the
    /// constant-time hash compare and the expiry/revoke check. Returns
    /// `Ok(None)` when no row exists in either the cache or SQLite.
    ///
    /// The cache is never populated with a `None` result: a flood of
    /// 401-with-unknown-prefix requests must not get a cache hit and
    /// must continue to hit SQLite each time. Caching negative results
    /// would also make it impossible to see a newly-minted token until
    /// the next TTL window.
    pub fn lookup(&self, prefix: &str) -> rusqlite::Result<Option<CachedToken>> {
        if let Some(cached) = self.inner.cache.get(prefix) {
            return Ok(Some(cached));
        }
        let row_and_hash = self.with_conn(|conn| store::lookup_by_prefix(conn, prefix))?;
        let Some((row, hash)) = row_and_hash else {
            return Ok(None);
        };
        let cached = CachedToken { row, hash };
        self.inner.cache.insert(prefix.to_string(), cached.clone());
        Ok(Some(cached))
    }

    /// Drop a specific prefix from the cache. Called from the admin
    /// revoke path so the revoke takes effect on the next request
    /// rather than waiting for TTL.
    pub fn invalidate_cached(&self, prefix: &str) {
        self.inner.cache.invalidate(prefix);
    }

    /// Drop every cached verification result. Used after a hot reload
    /// so a token revoked by an out-of-process actor (the Web Console,
    /// running in a separate `nanoguard-console` binary against the
    /// shared SQLite DB) takes effect on the very next request without
    /// waiting for the per-entry TTL. `nanoguard-console` triggers a
    /// reload after a token mutation specifically to flow through here.
    pub fn invalidate_all_cached(&self) {
        self.inner.cache.clear();
    }

    /// Test-only accessor for the cache, used to assert hit/miss
    /// behavior end-to-end without exposing the internal type publicly.
    #[cfg(test)]
    pub(crate) fn cache(&self) -> &TokenCache {
        &self.inner.cache
    }

    /// Acquire the SQLite connection. Held under a Mutex; callers should
    /// release it quickly. Used by the verification path and by admin
    /// endpoints for create/list/revoke.
    ///
    /// Recovers from a poisoned mutex by taking the inner connection
    /// anyway. Poisoning means another thread panicked while holding the
    /// lock; propagating it would brick every subsequent verification
    /// and admin call — one bad request would lock the whole table for
    /// the rest of the process lifetime. The SQLite connection state
    /// itself is fine across panics (rusqlite does not mutate it under
    /// our closures), so the recovery is safe.
    pub fn with_conn<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Connection) -> R,
    {
        let conn = self
            .inner
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        // Without this assertion the test was setting `require_https = true`
        // and silently never reading it back — a regression in the
        // accessor would have slipped through.
        assert!(ca.require_https());
    }

    #[test]
    fn open_rejects_non_alphanumeric_env_marker() {
        // The wire token shape requires the env-marker position to be
        // ASCII alphanumeric. A misconfigured marker would silently
        // mint tokens that fail every verification — fail at startup
        // instead.
        for bad in ['@', '#', '$', '!', '_', '-', ' ', '\t'] {
            let cfg = AuthConfig {
                env_marker: bad,
                ..AuthConfig::default()
            };
            let err = ClientAuth::open(":memory:", cfg)
                .err()
                .expect("non-alphanumeric env_marker should fail open()");
            assert!(
                format!("{err}").contains("env_marker"),
                "error for {:?} should mention env_marker; got: {err}",
                bad
            );
        }
    }

    #[test]
    fn open_accepts_valid_env_markers() {
        for good in ['p', 't', 's', '0', '9', 'X'] {
            let cfg = AuthConfig {
                env_marker: good,
                ..AuthConfig::default()
            };
            let _ca = ClientAuth::open(":memory:", cfg)
                .unwrap_or_else(|e| panic!("env_marker {:?} should succeed: {e}", good));
        }
    }

    #[test]
    fn lookup_caches_first_hit_and_serves_second_without_db() {
        // Insert directly via the store, then call lookup twice. The
        // first hit populates the cache; the second must return the
        // same data even after the row is deleted out from under us —
        // proving the second hit did not touch SQLite.
        let ca = in_memory(AuthConfig::default());
        let token = Token::generate('p');
        ca.with_conn(|c| store::insert(c, &token.prefix, &token.hash, 1, Some("x"), None).unwrap());

        let first = ca.lookup(&token.prefix).unwrap().expect("first lookup");
        assert_eq!(first.row.user_id, 1);

        // Wipe the DB row. If lookup were not cached, the next call
        // would return None.
        ca.with_conn(|c| {
            c.execute(
                "DELETE FROM client_tokens WHERE prefix = ?",
                [&token.prefix],
            )
            .unwrap();
        });

        let second = ca
            .lookup(&token.prefix)
            .unwrap()
            .expect("cache must serve the second lookup despite DB deletion");
        assert_eq!(second.row.user_id, 1);
    }

    #[test]
    fn invalidate_cached_forces_db_reread() {
        let ca = in_memory(AuthConfig::default());
        let token = Token::generate('p');
        ca.with_conn(|c| store::insert(c, &token.prefix, &token.hash, 7, None, None).unwrap());

        // Prime the cache.
        let _ = ca.lookup(&token.prefix).unwrap();
        // Drop the DB row, then invalidate. The next lookup re-reads
        // SQLite and sees the absence.
        ca.with_conn(|c| {
            c.execute(
                "DELETE FROM client_tokens WHERE prefix = ?",
                [&token.prefix],
            )
            .unwrap();
        });
        ca.invalidate_cached(&token.prefix);

        assert!(
            ca.lookup(&token.prefix).unwrap().is_none(),
            "invalidate must force a fresh DB read"
        );
    }

    #[test]
    fn lookup_miss_does_not_pollute_cache() {
        // An attacker hammering bogus prefixes should not be able to
        // grow the cache. Each miss must hit SQLite, but never insert
        // a negative entry.
        let ca = in_memory(AuthConfig::default());
        for i in 0..32 {
            let p = format!("ng_p_zz{:03}", i);
            assert!(ca.lookup(&p).unwrap().is_none());
        }
        assert_eq!(
            ca.cache().len(),
            0,
            "negative lookups must not populate the cache"
        );
    }

    #[test]
    fn with_conn_recovers_from_poisoned_mutex() {
        // Deliberately poison the inner mutex by panicking on a thread
        // that holds the lock. After the thread joins (with a panic),
        // with_conn must still serve queries — otherwise one bad request
        // would brick the entire client-auth subsystem for the rest of
        // the process lifetime.
        let ca = in_memory(AuthConfig::default());
        let ca_for_thread = ca.clone();
        let _ = std::thread::spawn(move || {
            let _guard = ca_for_thread.inner.conn.lock().unwrap();
            panic!("simulate a panic while holding the client_auth lock");
        })
        .join(); // joining the panicked thread returns Err(...), which we discard
        assert!(
            ca.inner.conn.is_poisoned(),
            "test setup: the mutex should be poisoned at this point"
        );

        // The point of the fix: with_conn should still work despite the
        // poisoned mutex.
        let n: i64 = ca.with_conn(|c| {
            c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))
                .expect("SELECT 1 succeeds after poison")
        });
        assert_eq!(n, 1);
    }
}
