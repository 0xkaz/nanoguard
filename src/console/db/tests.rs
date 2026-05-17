use super::*;
use rusqlite::Connection;

fn in_memory() -> Connection {
    Connection::open_in_memory().unwrap()
}

#[test]
fn user_crud_roundtrip() {
    let conn = in_memory();
    migrate(&conn).unwrap();

    let id = insert_user(
        &conn,
        "alice",
        Some("Alice"),
        Some("a@example.com"),
        "user",
        None,
    )
    .unwrap();
    assert_eq!(id, 1);

    let user = user_by_id(&conn, id).unwrap().expect("user exists");
    assert_eq!(user.username, "alice");
    assert_eq!(user.display_name, Some("Alice".into()));
    assert_eq!(user.role, "user");
    assert!(!user.disabled);

    let by_name = user_by_username(&conn, "alice")
        .unwrap()
        .expect("user by name");
    assert_eq!(by_name.id, id);

    update_user(
        &conn,
        id,
        UserUpdate {
            display_name: Some("Alice Smith"),
            role: Some("admin"),
            ..Default::default()
        },
    )
    .unwrap();
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
    let csrf = vec![2u8; 32];
    let expires = "2026-12-31T23:59:59Z";
    create_session(
        &conn,
        &sid,
        1,
        expires,
        Some("Mozilla"),
        Some("127.0.0.1"),
        &csrf,
    )
    .unwrap();

    let s = session_by_id(&conn, &sid).unwrap().expect("session exists");
    assert_eq!(s.user_id, 1);
    assert_eq!(s.csrf_token.as_deref(), Some(csrf.as_slice()));

    touch_session(&conn, &sid).unwrap();

    let new_csrf = vec![3u8; 32];
    let rotated = rotate_csrf_token(&conn, &sid, &new_csrf).unwrap();
    assert_eq!(rotated, 1);
    let after = session_by_id(&conn, &sid).unwrap().unwrap();
    assert_eq!(after.csrf_token.as_deref(), Some(new_csrf.as_slice()));

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

#[test]
fn login_attempt_tracking_and_pruning() {
    let conn = in_memory();
    migrate(&conn).unwrap();
    insert_user(&conn, "eve", None, None, "user", None).unwrap();

    record_login_attempt(&conn, "eve", Some("10.0.0.1")).unwrap();
    record_login_attempt(&conn, "eve", Some("10.0.0.1")).unwrap();

    let count = count_recent_login_attempts(&conn, "eve", 15).unwrap();
    assert_eq!(count, 2);

    let ip_count = count_recent_login_attempts_by_ip(&conn, "10.0.0.1", 15).unwrap();
    assert_eq!(ip_count, 2);

    clear_login_attempts(&conn, "eve").unwrap();
    let count_after = count_recent_login_attempts(&conn, "eve", 15).unwrap();
    assert_eq!(count_after, 0);

    // Prune old attempts
    record_login_attempt(&conn, "eve", Some("10.0.0.1")).unwrap();
    let pruned = prune_old_login_attempts(&conn, 1).unwrap();
    assert_eq!(pruned, 0); // too fresh
    let pruned_old = prune_old_login_attempts(&conn, 0).unwrap();
    assert_eq!(pruned_old, 1); // older than 0 minutes
}

#[test]
fn lockout_roundtrip() {
    let conn = in_memory();
    migrate(&conn).unwrap();
    let id = insert_user(&conn, "frank", None, None, "user", None).unwrap();

    let user = user_by_id(&conn, id).unwrap().unwrap();
    assert!(user.locked_until.is_none());

    let until = (chrono::Utc::now() + chrono::Duration::minutes(15)).to_rfc3339();
    set_locked_until(&conn, id, Some(&until)).unwrap();

    let user = user_by_id(&conn, id).unwrap().unwrap();
    assert_eq!(user.locked_until, Some(until));

    set_locked_until(&conn, id, None).unwrap();
    let user = user_by_id(&conn, id).unwrap().unwrap();
    assert!(user.locked_until.is_none());
}

#[test]
fn prune_idle_sessions_evicts_stale() {
    let conn = in_memory();
    migrate(&conn).unwrap();
    insert_user(&conn, "grace", None, None, "user", None).unwrap();

    let sid = vec![1u8; 32];
    let csrf = vec![2u8; 32];
    let expires = "2099-12-31T23:59:59Z";
    let old = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
    create_session(&conn, &sid, 1, expires, None, None, &csrf).unwrap();
    // Manually backdate last_seen_at
    conn.execute(
        "UPDATE user_sessions SET last_seen_at = ? WHERE id = ?",
        params![old, &sid],
    )
    .unwrap();

    let pruned = prune_idle_sessions(&conn, 1).unwrap();
    assert_eq!(pruned, 1);
    assert!(session_by_id(&conn, &sid).unwrap().is_none());
}
