//! In-memory verification cache for client tokens.
//!
//! Keeps the hot path off SQLite. Each authenticated request becomes a
//! single HashMap lookup + a constant-time hash compare. On miss the
//! middleware falls back to the SQLite store and populates the cache.
//!
//! Eviction has three triggers:
//!
//! 1. **TTL**. Entries older than `ttl` are treated as missing on lookup
//!    and dropped on insert. Bounds the staleness window for any
//!    revocation that bypasses `invalidate` (e.g., a direct SQLite
//!    edit) — within `ttl` seconds the next verification re-reads the
//!    DB and sees the revoke.
//! 2. **Explicit invalidation**. The admin revoke path calls
//!    [`TokenCache::invalidate`] for the affected prefix. Effective on
//!    the very next request — both producer (admin handler) and
//!    consumer (verifier) share the same `Arc<TokenCache>`, so there is
//!    no queue between them.
//! 3. **Capacity**. The map is bounded at `capacity`. On insert into a
//!    full cache we first sweep expired entries; if still full we evict
//!    the least-recently-used entry by `last_used` counter.
//!
//! The cache deliberately holds the full `ClientTokenRow` and the stored
//! hash. That makes the lookup self-contained: the middleware does the
//! constant-time hash check and the expiry/revoke check against cached
//! data, without re-touching SQLite. See `docs/design/client-auth.md > Caching`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::client_auth::store::ClientTokenRow;

/// One cached row + its stored hash. Cloned out of the cache on lookup
/// because the verification path holds the data across an `.await` and
/// we do not want to keep the cache lock across that.
#[derive(Clone)]
pub struct CachedToken {
    pub row: ClientTokenRow,
    pub hash: [u8; 32],
}

struct Entry {
    token: CachedToken,
    /// Wall-clock instant the entry was inserted. Compared against `ttl`
    /// on every lookup.
    inserted_at: Instant,
    /// Monotonic counter bumped on every read. Used as the LRU key when
    /// the cache is full and we have to choose an eviction victim. A
    /// counter is cheaper than a per-entry `Instant::now()` on the hot
    /// path and gives the same ordering.
    last_used: u64,
}

/// Bounded LRU+TTL cache keyed by token prefix.
///
/// Concurrent access is guarded by a single `Mutex`. Lookups and inserts
/// are both O(1) average; the only O(n) work happens when the cache is
/// full and an insert needs to find an eviction victim. That cost is
/// paid on the miss path, which is rare in steady state.
pub struct TokenCache {
    inner: Mutex<Inner>,
    capacity: usize,
    ttl: Duration,
}

struct Inner {
    map: HashMap<String, Entry>,
    /// Monotonically increasing counter used for LRU ordering. Wraparound
    /// would require ~58 years at 10 billion ops/sec; not a concern.
    counter: u64,
}

impl TokenCache {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(Inner {
                map: HashMap::with_capacity(capacity.min(1024)),
                counter: 0,
            }),
            capacity,
            ttl,
        }
    }

    /// Look up a token by prefix. Returns the cached row+hash on a fresh
    /// hit; `None` on miss or when the entry is past TTL.
    pub fn get(&self, prefix: &str) -> Option<CachedToken> {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        // Borrow the entry briefly to check TTL and bump last_used. If
        // expired, drop it and report miss.
        let expired = match inner.map.get(prefix) {
            Some(e) => e.inserted_at.elapsed() >= self.ttl,
            None => return None,
        };
        if expired {
            inner.map.remove(prefix);
            return None;
        }
        inner.counter = inner.counter.wrapping_add(1);
        let counter = inner.counter;
        let entry = inner.map.get_mut(prefix)?;
        entry.last_used = counter;
        Some(entry.token.clone())
    }

    /// Insert or replace an entry. Evicts to stay under capacity.
    pub fn insert(&self, prefix: String, token: CachedToken) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.counter = inner.counter.wrapping_add(1);
        let counter = inner.counter;

        // If the key already exists this is a refresh — just overwrite.
        if let Some(slot) = inner.map.get_mut(&prefix) {
            slot.token = token;
            slot.inserted_at = Instant::now();
            slot.last_used = counter;
            return;
        }

        // Bringing in a new key. Make room if we are at capacity.
        if inner.map.len() >= self.capacity {
            evict_one(&mut inner, self.ttl);
        }

        inner.map.insert(
            prefix,
            Entry {
                token,
                inserted_at: Instant::now(),
                last_used: counter,
            },
        );
    }

    /// Drop a specific prefix. Called by the admin revoke path so the
    /// revoke is visible on the next request rather than waiting for TTL.
    pub fn invalidate(&self, prefix: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.map.remove(prefix);
    }

    /// Clear the entire cache. For test setup only.
    #[cfg(test)]
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.map.clear();
    }

    /// Current entry count. Useful in tests.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .map
            .len()
    }

    /// Pairs with [`Self::len`] to satisfy `clippy::len_without_is_empty`.
    /// Test-only because both are test-only.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Make room for one new entry. Prefers to evict an expired entry; falls
/// back to the LRU victim if every slot is still fresh.
fn evict_one(inner: &mut Inner, ttl: Duration) {
    // First pass: drop any expired entry. Scanning O(n) is acceptable
    // because this only runs on a full-cache miss.
    let now = Instant::now();
    let expired_key = inner
        .map
        .iter()
        .find(|(_, e)| now.duration_since(e.inserted_at) >= ttl)
        .map(|(k, _)| k.clone());
    if let Some(k) = expired_key {
        inner.map.remove(&k);
        return;
    }

    // No expired entry — every slot is fresh. Evict the LRU one.
    let victim = inner
        .map
        .iter()
        .min_by_key(|(_, e)| e.last_used)
        .map(|(k, _)| k.clone());
    if let Some(k) = victim {
        inner.map.remove(&k);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(prefix: &str, id: i64) -> ClientTokenRow {
        ClientTokenRow {
            id,
            prefix: prefix.to_string(),
            user_id: 0,
            label: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            expires_at: None,
            revoked_at: None,
            last_used_at: None,
        }
    }

    fn token(prefix: &str, id: i64) -> CachedToken {
        CachedToken {
            row: row(prefix, id),
            hash: [0u8; 32],
        }
    }

    #[test]
    fn miss_on_unknown_prefix() {
        let cache = TokenCache::new(8, Duration::from_secs(60));
        assert!(cache.get("ng_p_xxxxx").is_none());
    }

    #[test]
    fn insert_then_get_returns_the_entry() {
        let cache = TokenCache::new(8, Duration::from_secs(60));
        cache.insert("ng_p_aaaaa".to_string(), token("ng_p_aaaaa", 1));
        let got = cache.get("ng_p_aaaaa").expect("cached");
        assert_eq!(got.row.id, 1);
    }

    #[test]
    fn invalidate_removes_the_entry() {
        let cache = TokenCache::new(8, Duration::from_secs(60));
        cache.insert("ng_p_aaaaa".to_string(), token("ng_p_aaaaa", 1));
        cache.invalidate("ng_p_aaaaa");
        assert!(cache.get("ng_p_aaaaa").is_none());
    }

    #[test]
    fn ttl_expires_entries() {
        // Short TTL so the test runs synchronously. The sleep is well
        // over the TTL so scheduler jitter under parallel test load
        // cannot leave the entry "still fresh".
        let cache = TokenCache::new(8, Duration::from_millis(10));
        cache.insert("ng_p_aaaaa".to_string(), token("ng_p_aaaaa", 1));
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            cache.get("ng_p_aaaaa").is_none(),
            "entry past TTL should not be returned"
        );
    }

    #[test]
    fn capacity_caps_the_cache() {
        let cache = TokenCache::new(2, Duration::from_secs(60));
        cache.insert("a".to_string(), token("a", 1));
        cache.insert("b".to_string(), token("b", 2));
        cache.insert("c".to_string(), token("c", 3));
        assert_eq!(cache.len(), 2, "third insert must evict to stay at cap");
    }

    #[test]
    fn eviction_prefers_lru_when_no_expired_entry() {
        // Capacity 2, TTL well in the future. Read `a` so it becomes the
        // most-recently-used; inserting a third entry must evict `b`.
        let cache = TokenCache::new(2, Duration::from_secs(60));
        cache.insert("a".to_string(), token("a", 1));
        cache.insert("b".to_string(), token("b", 2));
        let _ = cache.get("a");
        cache.insert("c".to_string(), token("c", 3));
        assert!(cache.get("a").is_some(), "MRU `a` must survive");
        assert!(cache.get("b").is_none(), "LRU `b` must be evicted");
        assert!(
            cache.get("c").is_some(),
            "freshly inserted `c` must be present"
        );
    }

    #[test]
    fn eviction_prefers_expired_over_lru() {
        // Insert `a`, wait past TTL, insert `b` (still fresh), insert
        // `c` into a full cache: the expired `a` is preferred over the
        // fresh-but-older `b`. Wider sleeps than the TTL so scheduler
        // jitter cannot land `a` inside its TTL window.
        let cache = TokenCache::new(2, Duration::from_millis(20));
        cache.insert("a".to_string(), token("a", 1));
        std::thread::sleep(Duration::from_millis(60));
        cache.insert("b".to_string(), token("b", 2));
        cache.insert("c".to_string(), token("c", 3));
        assert!(cache.get("b").is_some(), "fresh `b` survives");
        assert!(cache.get("c").is_some(), "new `c` is present");
        // `a` was expired anyway, but the contract is that the expired
        // slot is the one we reclaim before touching the LRU slot.
        assert!(cache.get("a").is_none(), "expired `a` was evicted");
    }

    #[test]
    fn insert_on_existing_key_refreshes_ttl_in_place() {
        // Re-inserting the same key should reset `inserted_at`, so the
        // entry is not considered expired right after the refresh. Use
        // a comfortably wide TTL so the assertion does not depend on
        // tight scheduling — what we are actually checking is the
        // refresh, not the parser's reaction to wall clock jitter.
        let cache = TokenCache::new(4, Duration::from_millis(200));
        cache.insert("a".to_string(), token("a", 1));
        std::thread::sleep(Duration::from_millis(50));
        cache.insert("a".to_string(), token("a", 1));
        // Lookup immediately after the refresh: definitely inside the
        // 200ms window. If the refresh failed to update `inserted_at`
        // the entry would still appear fresh too (50ms < 200ms), so we
        // need a separate longer wait below.
        assert!(
            cache.get("a").is_some(),
            "refresh keeps the entry available"
        );

        // Now wait past the *original* TTL window but inside the
        // refreshed one. 175ms after the refresh = 225ms after the
        // original insert: past 200ms original TTL, well inside the
        // refreshed 200ms window.
        std::thread::sleep(Duration::from_millis(175));
        assert!(
            cache.get("a").is_some(),
            "refresh resets the TTL clock — entry past original TTL still cached"
        );
    }

    #[test]
    fn recovers_from_poisoned_mutex() {
        // Same recovery guarantee as ClientAuth.with_conn: a poisoned
        // cache must still serve requests, otherwise one panic anywhere
        // would brick auth for the rest of the process.
        let cache = std::sync::Arc::new(TokenCache::new(4, Duration::from_secs(60)));
        let cache_for_thread = std::sync::Arc::clone(&cache);
        let _ = std::thread::spawn(move || {
            let _guard = cache_for_thread.inner.lock().unwrap();
            panic!("poison the cache");
        })
        .join();
        assert!(cache.inner.is_poisoned(), "test setup: mutex is poisoned");
        // The whole point of the unwrap_or_else: still works.
        cache.insert("a".to_string(), token("a", 1));
        assert!(cache.get("a").is_some());
    }
}
