use super::*;

const PEER_IP: std::net::IpAddr = std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 50));

fn peer() -> PeerKey {
    PeerKey::of(Some(PEER_IP))
}

fn store(dir: &Path) -> DashboardPin {
    DashboardPin::with_path(dir.join("dashboard-pin.json"))
}

#[test]
fn absent_file_is_unset() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    assert!(!s.is_set());
    assert_eq!(
        s.status(peer(), 100.0),
        PinStatus {
            pin_set: false,
            locked: false,
            locked_until: 0.0,
            node_locked: false,
        }
    );
    assert_eq!(s.verify_pin(peer(), "1234", 100.0), VerifyOutcome::NotSet);
    assert!(s.salt().is_none());
}

/// The node-wide budget: wrong PINs sprayed from many addresses, each under
/// its own per-caller threshold, lock every caller once the budget is spent,
/// and only a clear lifts it.
#[test]
fn a_spray_from_many_peers_locks_the_whole_node() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    s.set_pin("4321", 0.0).unwrap();
    let peer_n = |n: u8| PeerKey::of(Some(std::net::IpAddr::from([10, 0, 1, n])));
    let mut engaged = 0;
    for n in 1..=40u8 {
        match s.verify_pin(peer_n(n), "0000", f64::from(n)) {
            VerifyOutcome::NodeLocked { engaged: true, .. } => engaged += 1,
            VerifyOutcome::NodeLocked { engaged: false, .. } => assert!(n > 30),
            VerifyOutcome::Wrong { .. } => assert!(n < NODE_FAILURE_BUDGET as u8),
            other => panic!("unexpected {other:?} for peer {n}"),
        }
    }
    assert_eq!(engaged, 1, "the lock engages exactly once");
    // The 41st peer is locked out even with the right PIN.
    match s.verify_pin(peer_n(41), "4321", 41.0) {
        VerifyOutcome::NodeLocked { locked_until, .. } => {
            assert_eq!(locked_until, 30.0 + NODE_LOCK_S)
        }
        other => panic!("expected NodeLocked, got {other:?}"),
    }
    let status = s.status(peer_n(41), 41.0);
    assert!(status.locked && status.node_locked);
    // A clear lifts it (the key holder's or the device's reset).
    s.clear().unwrap();
    s.set_pin("4321", 50.0).unwrap();
    assert_eq!(s.verify_pin(peer_n(41), "4321", 51.0), VerifyOutcome::Ok);
}

/// The budget is a rolling hour: failures older than the window do not count.
#[test]
fn the_node_budget_forgets_failures_older_than_an_hour() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    s.set_pin("4321", 0.0).unwrap();
    let peer_n = |n: u8| PeerKey::of(Some(std::net::IpAddr::from([10, 0, 2, n])));
    for n in 1..NODE_FAILURE_BUDGET as u8 {
        s.verify_pin(peer_n(n), "0000", f64::from(n));
    }
    // An hour later the earlier failures have aged out.
    let later = NODE_WINDOW_S + 100.0;
    assert!(matches!(
        s.verify_pin(peer_n(200), "0000", later),
        VerifyOutcome::Wrong { .. }
    ));
    assert_eq!(
        s.verify_pin(peer_n(201), "4321", later + 1.0),
        VerifyOutcome::Ok
    );
}

/// The record is cached between reads, but a change on disk is still seen once
/// the cache lapses.
#[test]
fn the_record_cache_follows_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    s.set_pin("1234", 0.0).unwrap();
    assert!(s.is_set());
    // Removed behind the store's back: the cached record stands until the TTL,
    // then the stamp change is noticed.
    std::fs::remove_file(s.path()).unwrap();
    assert!(s.is_set(), "within the TTL the cached record stands");
    if let Some(c) = s.doc_cache.lock().as_mut() {
        c.at = Instant::now().checked_sub(DOC_TTL).unwrap();
    }
    assert!(!s.is_set());
}

#[test]
fn set_then_verify_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    s.set_pin("1234", 10.0).unwrap();
    assert!(s.is_set());
    assert_eq!(s.verify_pin(peer(), "1234", 11.0), VerifyOutcome::Ok);
    // A wrong PIN is Wrong with the remaining countdown, not Ok.
    assert_eq!(
        s.verify_pin(peer(), "0000", 12.0),
        VerifyOutcome::Wrong {
            remaining_attempts: LOCK_AFTER - 1
        }
    );
    // A subsequent correct PIN resets the counter.
    assert_eq!(s.verify_pin(peer(), "1234", 13.0), VerifyOutcome::Ok);
    assert_eq!(
        s.verify_pin(peer(), "0000", 14.0),
        VerifyOutcome::Wrong {
            remaining_attempts: LOCK_AFTER - 1
        }
    );
}

#[test]
fn rejects_non_digit_and_short_pins() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    assert!(matches!(s.set_pin("12", 0.0), Err(PinError::InvalidPin)));
    assert!(matches!(s.set_pin("12ab", 0.0), Err(PinError::InvalidPin)));
    assert!(matches!(s.set_pin("", 0.0), Err(PinError::InvalidPin)));
    assert!(s.set_pin("1234", 0.0).is_ok());
    assert!(s.set_pin("123456", 0.0).is_ok());
}

#[test]
fn lockout_after_five_wrong() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    s.set_pin("4321", 0.0).unwrap();
    // Four wrong: still counting down, not locked.
    for i in 1..=4u32 {
        assert_eq!(
            s.verify_pin(peer(), "0000", i as f64),
            VerifyOutcome::Wrong {
                remaining_attempts: LOCK_AFTER - i
            }
        );
    }
    // Fifth wrong: locked for 30 s.
    match s.verify_pin(peer(), "0000", 5.0) {
        VerifyOutcome::Locked { locked_until } => assert_eq!(locked_until, 35.0),
        other => panic!("expected Locked, got {other:?}"),
    }
    // While locked, even the CORRECT PIN is refused (returns Locked, no
    // attempt consumed).
    match s.verify_pin(peer(), "4321", 10.0) {
        VerifyOutcome::Locked { locked_until } => assert_eq!(locked_until, 35.0),
        other => panic!("expected Locked, got {other:?}"),
    }
    // After the window, the correct PIN unlocks + resets.
    assert_eq!(s.verify_pin(peer(), "4321", 40.0), VerifyOutcome::Ok);
    assert_eq!(
        s.verify_pin(peer(), "0000", 41.0),
        VerifyOutcome::Wrong {
            remaining_attempts: LOCK_AFTER - 1
        }
    );
}

/// One caller's wrong PINs lock that caller only. A single shared ladder let
/// any host keep the operator locked out by sending one wrong PIN each time
/// the lock expired.
#[test]
fn a_lockout_is_per_caller() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    s.set_pin("4321", 0.0).unwrap();
    let other = PeerKey::of(Some("192.168.1.51".parse().unwrap()));
    for i in 1..=LOCK_AFTER {
        s.verify_pin(other, "0000", i as f64);
    }
    assert!(s.status(other, 6.0).locked);
    assert!(!s.status(peer(), 6.0).locked, "the operator is not locked");
    assert_eq!(s.verify_pin(peer(), "4321", 6.0), VerifyOutcome::Ok);
}

/// Every address in one IPv6 /64 shares a ladder, and an IPv4-mapped
/// address shares its IPv4 ladder.
#[test]
fn peer_keys_collapse_a_v6_prefix_and_v4_mapped_addresses() {
    let a = PeerKey::of(Some("2001:db8:1:2::10".parse().unwrap()));
    let b = PeerKey::of(Some("2001:db8:1:2:ffff::1".parse().unwrap()));
    let c = PeerKey::of(Some("2001:db8:1:3::10".parse().unwrap()));
    assert_eq!(a, b);
    assert_ne!(a, c);
    let mapped = PeerKey::of(Some("::ffff:192.168.1.50".parse().unwrap()));
    assert_eq!(mapped, peer());
}

/// Concurrent wrong attempts each count: the verify is one critical section.
#[test]
fn concurrent_wrong_attempts_all_count() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    s.set_pin("4321", 0.0).unwrap();
    let handles: Vec<_> = (0..LOCK_AFTER)
        .map(|_| {
            let s = s.clone();
            std::thread::spawn(move || s.verify_pin(peer(), "0000", 1.0))
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert!(s.status(peer(), 1.0).locked, "five wrong attempts lock");
}

/// The edge validates a session on every request, so the salt is cached;
/// a set through the store must still revoke at once.
#[test]
fn a_set_invalidates_the_cached_salt_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    s.set_pin("1234", 0.0).unwrap();
    let before = s.salt().unwrap();
    s.set_pin("1234", 1.0).unwrap();
    assert_ne!(s.salt().unwrap(), before);
    s.clear().unwrap();
    assert!(s.salt().is_none());
}

#[test]
fn clear_removes_and_re_tofus() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    s.set_pin("1234", 0.0).unwrap();
    assert!(s.is_set());
    s.clear().unwrap();
    assert!(!s.is_set());
    // Clearing an absent file is a no-op success.
    s.clear().unwrap();
}

#[test]
fn set_rotates_the_salt() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    s.set_pin("1234", 0.0).unwrap();
    let salt1 = s.salt().unwrap();
    s.set_pin("1234", 1.0).unwrap();
    let salt2 = s.salt().unwrap();
    assert_ne!(salt1, salt2, "each set mints a fresh salt");
}

#[test]
fn session_round_trip_and_reset_revocation() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    s.set_pin("1234", 0.0).unwrap();
    let paired = Pairing::Paired("ados_key".to_string());
    let sess = s
        .mint_session("ados_key")
        .expect("a set PIN mints a session");
    assert!(
        s.session_valid_for(&paired, &sess.token),
        "a fresh session verifies"
    );
    // A reset (new salt) revokes the live session.
    s.set_pin("5678", 1.0).unwrap();
    assert!(
        !s.session_valid_for(&paired, &sess.token),
        "resetting the PIN revokes the prior session"
    );
    // Unpaired never validates a session (the data plane is open anyway).
    assert!(!s.session_valid_for(&Pairing::Unpaired, &sess.token));
    // A wrong pairing key does not validate.
    let other = Pairing::Paired("different".to_string());
    let fresh = s.mint_session("ados_key").unwrap();
    assert!(!s.session_valid_for(&other, &fresh.token));
}

/// The PIN gate on an unpaired node's data routes: the session is minted under
/// the empty-key issuer (there is no pairing key on file) and must validate
/// via `session_valid_unpaired`. This is the credential a trusted-LAN browser
/// holds before the node is paired.
#[test]
fn unpaired_session_round_trips_and_revokes_on_reset() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    // No PIN set: nothing can validate, and nothing can be minted.
    assert!(!s.session_valid_unpaired("v1|1|2|00"));
    assert!(s.mint_session("").is_none());

    s.set_pin("1234", 0.0).unwrap();
    let sess = s
        .mint_session("")
        .expect("a set PIN mints an unpaired session");
    assert!(
        s.session_valid_unpaired(&sess.token),
        "a fresh unpaired session verifies"
    );
    // A reset (new salt) revokes the live session.
    s.set_pin("5678", 1.0).unwrap();
    assert!(
        !s.session_valid_unpaired(&sess.token),
        "resetting the PIN revokes the prior unpaired session"
    );
    // A tampered token never validates.
    assert!(!s.session_valid_unpaired("v1|1|2|ff"));
    assert!(!s.session_valid_unpaired("garbage"));
}

#[cfg(unix)]
#[test]
fn record_is_0600() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    s.set_pin("1234", 0.0).unwrap();
    let mode = std::fs::metadata(s.path()).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "dashboard-pin.json must be 0600");
}
