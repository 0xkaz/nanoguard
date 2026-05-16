use super::*;
use crate::client_auth::{sha256_of, Token};

fn fresh_db() -> Connection {
    let conn = Connection::open_in_memory().expect("open in-memory");
    migrate(&conn).expect("migrate");
    conn
}

#[test]
fn migrate_is_idempotent() {
    let conn = fresh_db();
    // Calling migrate again on the same connection must not error.
    migrate(&conn).expect("second migrate");
}

#[test]
fn insert_then_lookup_round_trips() {
    let conn = fresh_db();
    let t = Token::generate('p');
    let id = insert(&conn, &t.prefix, &t.hash, 1, Some("interactive"), None).expect("insert");
    assert!(id > 0);

    let (row, stored_hash) = lookup_by_prefix(&conn, &t.prefix)
        .expect("lookup")
        .expect("row exists");
    assert_eq!(row.id, id);
    assert_eq!(row.prefix, t.prefix);
    assert_eq!(row.user_id, 1);
    assert_eq!(row.label.as_deref(), Some("interactive"));
    assert!(row.revoked_at.is_none());
    assert_eq!(stored_hash, t.hash);
}

#[test]
fn lookup_missing_prefix_returns_none() {
    let conn = fresh_db();
    let r = lookup_by_prefix(&conn, "ng_p_nope1").expect("lookup");
    assert!(r.is_none());
}

#[test]
fn duplicate_prefix_insert_fails() {
    let conn = fresh_db();
    let t = Token::generate('p');
    insert(&conn, &t.prefix, &t.hash, 1, None, None).expect("first insert");
    let err = insert(&conn, &t.prefix, &t.hash, 1, None, None).unwrap_err();
    // SQLite UNIQUE violation.
    assert!(matches!(err, rusqlite::Error::SqliteFailure(_, _)));
}

#[test]
fn list_for_user_returns_only_their_tokens() {
    let conn = fresh_db();
    let a = Token::generate('p');
    let b = Token::generate('p');
    let c = Token::generate('p');
    insert(&conn, &a.prefix, &a.hash, 1, Some("a"), None).unwrap();
    insert(&conn, &b.prefix, &b.hash, 1, Some("b"), None).unwrap();
    insert(&conn, &c.prefix, &c.hash, 2, Some("c"), None).unwrap();

    let alice = list_for_user(&conn, 1).expect("list");
    assert_eq!(alice.len(), 2);
    let labels: Vec<_> = alice.iter().filter_map(|r| r.label.as_deref()).collect();
    assert!(labels.contains(&"a"));
    assert!(labels.contains(&"b"));

    let bob = list_for_user(&conn, 2).expect("list");
    assert_eq!(bob.len(), 1);
    assert_eq!(bob[0].label.as_deref(), Some("c"));
}

#[test]
fn revoke_sets_timestamp_and_is_idempotent() {
    let conn = fresh_db();
    let t = Token::generate('p');
    let id = insert(&conn, &t.prefix, &t.hash, 1, None, None).unwrap();

    let n = revoke(&conn, id).expect("revoke");
    assert_eq!(n, 1);

    let (row, _) = lookup_by_prefix(&conn, &t.prefix).unwrap().unwrap();
    let first_revoked = row.revoked_at.clone().expect("revoked_at set");
    assert!(!first_revoked.is_empty());

    // Second revoke is a no-op (no matching row because the WHERE
    // filter excludes already-revoked rows).
    let n2 = revoke(&conn, id).expect("second revoke");
    assert_eq!(n2, 0);

    let (row2, _) = lookup_by_prefix(&conn, &t.prefix).unwrap().unwrap();
    assert_eq!(row2.revoked_at, Some(first_revoked));
}

#[test]
fn hash_blob_corruption_is_caught() {
    // Insert a deliberately wrong-length blob and check that lookup
    // surfaces a clear error rather than silently accepting it.
    let conn = fresh_db();
    conn.execute(
        "INSERT INTO client_tokens (prefix, hash, user_id, created_at)
         VALUES (?, ?, ?, ?)",
        rusqlite::params!["ng_p_corrupt", &[0u8; 16][..], 0, "2026-01-01T00:00:00Z"],
    )
    .unwrap();

    let err = lookup_by_prefix(&conn, "ng_p_corrupt").unwrap_err();
    assert!(matches!(err, rusqlite::Error::FromSqlConversionFailure(..)));
}

#[test]
fn stored_hash_matches_sha256_of_wire() {
    // Belt-and-suspenders: the integration boundary between Token and
    // the store must not silently corrupt the hash.
    let conn = fresh_db();
    let t = Token::generate('p');
    insert(&conn, &t.prefix, &t.hash, 1, None, None).unwrap();
    let (_, stored) = lookup_by_prefix(&conn, &t.prefix).unwrap().unwrap();
    assert_eq!(stored, sha256_of(&t.wire));
}
