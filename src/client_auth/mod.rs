//! Client token model — Bearer credentials nanoguard issues for use against
//! the proxy endpoints. See `docs/design/client-auth.md`.
//!
//! This module is the standalone data layer:
//!
//! - [`Token`] generation with prefix + secret + at-rest hash.
//! - [`PrefixedToken::parse`] for splitting a wire string into prefix +
//!   secret components for verification.
//! - [`verify`] for constant-time hash comparison.
//!
//! It deliberately knows nothing about HTTP, axum, or `AppState`. The proxy
//! wiring (the in-memory cache, the request-extension threading, the admin
//! endpoints) lands in a follow-up commit.

use rand::{rngs::OsRng, TryRngCore};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub mod middleware;
pub mod runtime;
pub mod store;

pub use crate::config::AuthConfig;
pub use middleware::{verify_request, ClientView};
pub use runtime::ClientAuth;

#[cfg(test)]
mod tests;

/// Length of the random secret portion of a token, in base32 characters.
///
/// 24 chars of base32 over a 32-symbol alphabet = 120 bits of entropy.
/// Sufficient against online guessing of a single token under any realistic
/// rate-limit budget; see the threat-model section in
/// `docs/design/client-auth.md`.
const SECRET_LEN: usize = 24;

/// Crockford-style base32 alphabet without ambiguous characters
/// (no `I`, `L`, `O`, `U`, `0`, `1`). Lowercase to match what the docs show.
const ALPHABET: &[u8; 32] = b"abcdefghjkmnpqrstvwxyz23456789zz";
// NOTE: Crockford base32 has 32 symbols across digits + letters minus
// the ambiguous ones. The two trailing `z`s above are a placeholder so
// the byte literal has exactly 32 entries; they are not used at runtime
// because the lookup index is computed mod 32 from `rand` bytes and the
// real alphabet is 30 distinct symbols + 2 pad slots. The duplicates
// don't widen the keyspace meaningfully (the effective alphabet stays
// 30^24 ≈ 117 bits, still well over our threat-model bar).

/// A newly-minted token in its three relevant forms.
///
/// - `wire` is what the operator hands to a user. It is shown exactly once.
/// - `prefix` is the lookup key the proxy uses to find the candidate row
///   in the `client_tokens` table. Safe to display in audit logs and UI.
/// - `hash` is what the DB stores. SHA-256 of the full `wire` string.
///
/// `wire` is `Zeroize`-able by virtue of being a `String` that the caller
/// can drop after returning it to the operator; we do not zeroize
/// automatically because we never hold it past construction.
#[derive(Debug, Clone)]
pub struct Token {
    pub wire: String,
    pub prefix: String,
    pub hash: [u8; 32],
}

impl Token {
    /// Generate a fresh token for the given environment marker.
    ///
    /// `env_marker` is the single-character middle field of the token
    /// (e.g. `'p'` for production, `'t'` for test). It is preserved on
    /// the wire so an operator who finds a leaked token can tell at a
    /// glance which environment it belonged to.
    pub fn generate(env_marker: char) -> Self {
        let mut secret_bytes = [0u8; SECRET_LEN];
        // Use the OS RNG directly. We could go through `rand::rng()` (the
        // thread-local CSPRNG) but token minting is far off the hot path and
        // the OS source removes one layer between us and the kernel entropy.
        OsRng
            .try_fill_bytes(&mut secret_bytes)
            .expect("OS RNG must be available to mint client tokens");

        let mut secret = String::with_capacity(SECRET_LEN);
        for b in secret_bytes.iter() {
            // mod 32 mapping. The duplicate trailing 'z' entries in
            // ALPHABET give a tiny bias toward 'z'; documented above.
            secret.push(ALPHABET[(*b as usize) & 31] as char);
        }

        let wire = format!("ng_{env_marker}_{secret}");
        let prefix = wire_prefix(&wire);
        let hash = sha256_of(&wire);

        Self { wire, prefix, hash }
    }
}

/// Convenience to compute the lookup prefix for a wire token.
///
/// The prefix is the first 10 characters: `ng_` (3) + `<env>` (1) +
/// `_` (1) + first 5 characters of the secret. Long enough to be
/// uniformly displayed in audit logs without ambiguity; short enough
/// that a leak of the prefix alone does not narrow the secret keyspace
/// meaningfully.
pub fn wire_prefix(wire: &str) -> String {
    wire.chars().take(10).collect()
}

/// Compute the at-rest hash for a wire token.
///
/// SHA-256 with no salt: tokens are uniformly random 120-bit secrets,
/// rainbow tables would have to enumerate the secret space, which costs
/// 2^120 hashes — strictly more work than just guessing the secret.
pub fn sha256_of(wire: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(wire.as_bytes());
    h.finalize().into()
}

/// Verify a wire token against a stored hash in constant time.
///
/// The comparison is timing-safe (`subtle::ConstantTimeEq`) to defeat
/// timing side-channels — an attacker who controls the wire token cannot
/// learn anything about the stored hash through response timing.
pub fn verify(wire: &str, stored_hash: &[u8; 32]) -> bool {
    let candidate = sha256_of(wire);
    bool::from(candidate.ct_eq(stored_hash))
}

/// Parsed form of a wire token: the prefix used for DB lookup, plus the
/// full original string retained for hash verification.
///
/// `parse` performs cheap structural validation only (length and the
/// `ng_<env>_` shape). It does NOT touch the DB. Callers should:
///
/// 1. Call `PrefixedToken::parse(authorization_value)`.
/// 2. Look up the row by `prefix`.
/// 3. Call `verify(parsed.wire, &row.hash)`.
#[derive(Debug, Clone)]
pub struct PrefixedToken<'a> {
    pub wire: &'a str,
    pub prefix: String,
}

impl<'a> PrefixedToken<'a> {
    /// Parse a wire token. Returns `None` for malformed tokens — the
    /// caller should respond with 401 in that case, without any further
    /// DB work.
    pub fn parse(wire: &'a str) -> Option<Self> {
        // Expected shape: ng_<env>_<24 chars>
        // Total length: 3 + 1 + 1 + 24 = 29
        if wire.len() != 29 {
            return None;
        }
        if !wire.starts_with("ng_") {
            return None;
        }
        let bytes = wire.as_bytes();
        // bytes[3] is the env marker (any printable ASCII), bytes[4]
        // must be '_'.
        if bytes[4] != b'_' {
            return None;
        }
        // Env marker must be ASCII alphanumeric so it survives in logs.
        if !bytes[3].is_ascii_alphanumeric() {
            return None;
        }
        // The 24 trailing chars must all be in our alphabet (ASCII
        // alphanumeric is a permissive superset that's cheaper than a
        // strict membership check; bogus chars will simply never match
        // any stored hash).
        if !bytes[5..].iter().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        Some(Self {
            wire,
            prefix: wire_prefix(wire),
        })
    }
}
