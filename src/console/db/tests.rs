use rusqlite::Connection;
use super::*;

fn in_memory() -> Connection {
    Connection::open_in_memory().unwrap()
}

#[test]
fn user_crud_roundtrip() {
    let conn = in_memory();
    migrate(&conn).unwrap();

    let id = insert_user(&conn, "alice", Some("Alice"), Some("a@example.com"), "user", None).unwrap();
    assert_eq!(id, 1);

    let user = user_by_id(&conn, id).unwrap().expect("user exists");
    assert_eq!(user.username, "alice");
    assert_eq!(user.display_name, Some("Alice".into()));
    assert_eq!(user.role, "user");
    assert!(!user.disabled);

    let by_name = user_by_username(&conn, "alice").unwrap().expect("user by name");
    assert_eq!(by_name.id, id);

    update_user(&conn, id, Some("Alice Smith"), None, Some("admin"), None, None, None).unwrap();
    let updated = user_by_id(&conn, id).unwrap().unwrap();
    assert_eq!(updated.display_name, Some("Alice Smith".into()));
    assert_eq!(updated.role, "admin");
}

#[test]
fn session_lifecycle() {
    let conn = in_memory();
    migrate(&conn).unwrap();
    insert_user(&conn, "bob", None, None, "user", None).unwrap();

    let sid = vec![1u8; 32];
    let expires = "2026-12-31T23:59:59Z";
    create_session(&conn, &sid, 1, expires, Some("Mozilla"), Some("127.0.0.1")).unwrap();

    let s = session_by_id(&conn, &sid).unwrap().expect("session exists");
    assert_eq!(s.user_id, 1);

    touch_session(&conn, &sid).unwrap();

    let pruned = prune_expired_sessions(&conn).unwrap();
    assert_eq!(pruned, 0);

    delete_session(&conn, &sid).unwrap();
    assert!(session_by_id(&conn, &sid).unwrap().is_none());
}

#[test]
fn bootstrap_only_when_empty() {
    let conn = in_memory();
    migrate(&conn).unwrap();

    let hash = b"fake-hash".to_vec();
    let created = maybe_bootstrap_admin(&conn, "admin", &hash).unwrap();
    assert!(created);

    let created2 = maybe_bootstrap_admin(&conn, "admin2", &hash).unwrap();
    assert!(!created2);
}
