use super::*;

#[test]
fn generate_produces_well_formed_wire() {
    let t = Token::generate('p');
    assert_eq!(t.wire.len(), 29);
    assert!(t.wire.starts_with("ng_p_"));
    // The 24-char secret tail is all ASCII alphanumeric.
    assert!(t.wire[5..].chars().all(|c| c.is_ascii_alphanumeric()));
}

#[test]
fn generate_prefix_is_first_10_chars() {
    let t = Token::generate('p');
    assert_eq!(t.prefix.len(), 10);
    assert!(t.wire.starts_with(&t.prefix));
}

#[test]
fn generate_hash_matches_sha256_of_wire() {
    let t = Token::generate('t');
    assert_eq!(t.hash, sha256_of(&t.wire));
}

#[test]
fn two_generated_tokens_differ() {
    // Cheap birthday-paradox sanity check: two fresh tokens at 120 bits
    // of entropy should virtually never collide.
    let a = Token::generate('p');
    let b = Token::generate('p');
    assert_ne!(a.wire, b.wire);
    assert_ne!(a.hash, b.hash);
}

#[test]
fn env_marker_is_preserved_in_wire_and_prefix() {
    let p = Token::generate('p');
    let t = Token::generate('t');
    assert!(p.wire.starts_with("ng_p_"));
    assert!(t.wire.starts_with("ng_t_"));
    assert!(p.prefix.starts_with("ng_p_"));
    assert!(t.prefix.starts_with("ng_t_"));
}

#[test]
fn verify_accepts_matching_wire() {
    let t = Token::generate('p');
    assert!(verify(&t.wire, &t.hash));
}

#[test]
fn verify_rejects_tampered_wire() {
    let t = Token::generate('p');
    let mut bad = t.wire.clone();
    // Flip the last character to anything different.
    let last = bad.pop().unwrap();
    bad.push(if last == 'z' { 'a' } else { 'z' });
    assert!(!verify(&bad, &t.hash));
}

#[test]
fn verify_rejects_wrong_hash() {
    let a = Token::generate('p');
    let b = Token::generate('p');
    assert!(!verify(&a.wire, &b.hash));
}

#[test]
fn parse_accepts_well_formed() {
    let t = Token::generate('p');
    let parsed = PrefixedToken::parse(&t.wire).expect("should parse");
    assert_eq!(parsed.prefix, t.prefix);
    assert_eq!(parsed.wire, t.wire);
}

#[test]
fn parse_rejects_wrong_prefix() {
    assert!(PrefixedToken::parse("xx_p_aaaaaaaaaaaaaaaaaaaaaaaa").is_none());
}

#[test]
fn parse_rejects_missing_underscore() {
    // 29 chars total but the position-4 separator is wrong.
    assert!(PrefixedToken::parse("ng_pXaaaaaaaaaaaaaaaaaaaaaaaa").is_none());
}

#[test]
fn parse_rejects_short_token() {
    assert!(PrefixedToken::parse("ng_p_short").is_none());
}

#[test]
fn parse_rejects_long_token() {
    assert!(PrefixedToken::parse("ng_p_aaaaaaaaaaaaaaaaaaaaaaaaEXTRA").is_none());
}

#[test]
fn parse_rejects_non_alphanumeric_secret() {
    // A '-' in the secret portion.
    assert!(PrefixedToken::parse("ng_p_aaaaaaaaaaa-aaaaaaaaaaaa").is_none());
}

#[test]
fn parse_rejects_non_alphanumeric_env_marker() {
    // A space where the env marker should be.
    assert!(PrefixedToken::parse("ng_ _aaaaaaaaaaaaaaaaaaaaaaaa").is_none());
}

#[test]
fn wire_prefix_is_idempotent_for_already_short_input() {
    // Defensive: passing a too-short wire should not panic; takes(10)
    // simply yields fewer characters.
    let p = wire_prefix("abc");
    assert_eq!(p, "abc");
}
