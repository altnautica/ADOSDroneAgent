use super::*;

#[test]
fn cursor_round_trips_under_a_matching_fingerprint() {
    let c = Cursor::new(1_700_000_000_000_000, 42, 0xDEAD_BEEF);
    let token = c.encode();
    let back = Cursor::decode(&token, 0xDEAD_BEEF).unwrap();
    assert_eq!(back, c);
}

#[test]
fn cursor_is_rejected_under_a_different_fingerprint() {
    let c = Cursor::new(1, 2, 0xAAAA);
    let token = c.encode();
    let err = Cursor::decode(&token, 0xBBBB).unwrap_err();
    assert_eq!(err, CursorError::Fingerprint);
}

#[test]
fn a_garbage_token_is_rejected_cleanly() {
    assert_eq!(
        Cursor::decode("!!!not base64!!!", 0).unwrap_err(),
        CursorError::Base64
    );
    // Valid base64 that is not the cursor shape.
    let not_a_cursor = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"hello");
    assert_eq!(
        Cursor::decode(&not_a_cursor, 0).unwrap_err(),
        CursorError::Decode
    );
}

#[test]
fn fingerprint_is_stable_and_order_sensitive() {
    let mut a = FilterFingerprint::new();
    a.add_str("logs")
        .add_opt_i64(Some(100))
        .add_str_list(&["api".into(), "video".into()]);
    let mut b = FilterFingerprint::new();
    b.add_str("logs")
        .add_opt_i64(Some(100))
        .add_str_list(&["api".into(), "video".into()]);
    assert_eq!(
        a.finish(),
        b.finish(),
        "same inputs give the same fingerprint"
    );

    let mut c = FilterFingerprint::new();
    c.add_str("logs")
        .add_opt_i64(Some(100))
        .add_str_list(&["video".into(), "api".into()]);
    assert_ne!(
        a.finish(),
        c.finish(),
        "reordered list changes the fingerprint"
    );
}

#[test]
fn fingerprint_distinguishes_absent_from_present() {
    let mut a = FilterFingerprint::new();
    a.add_opt_i64(None);
    let mut b = FilterFingerprint::new();
    b.add_opt_i64(Some(0));
    assert_ne!(
        a.finish(),
        b.finish(),
        "absent must differ from a zero value"
    );
}

#[test]
fn length_prefix_prevents_boundary_collisions() {
    let mut a = FilterFingerprint::new();
    a.add_str("ab").add_str("c");
    let mut b = FilterFingerprint::new();
    b.add_str("a").add_str("bc");
    assert_ne!(a.finish(), b.finish(), "split point must matter");
}
