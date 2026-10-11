use super::*;

fn claim(id: &str, ticket: Option<&str>) -> InjectorClaim {
    InjectorClaim {
        client_id: id.to_string(),
        ticket: ticket.map(str::to_string),
    }
}

// ── verification against the pairing key ─────────────────────────────────

#[test]
fn unpaired_node_accepts_the_asserted_id() {
    let dir = tempfile::tempdir().unwrap();
    let pairing = dir.path().join("pairing.json");
    // No pairing file → Unpaired → asserted id accepted, no ticket needed.
    assert_eq!(
        verify_injector(&pairing, &claim("ai-mission", None)),
        Some("ai-mission".to_string())
    );
}

#[test]
fn paired_node_rejects_a_missing_or_bad_ticket() {
    let dir = tempfile::tempdir().unwrap();
    let pairing = dir.path().join("pairing.json");
    std::fs::write(&pairing, r#"{"paired": true, "api_key": "k-secret"}"#).unwrap();
    assert_eq!(verify_injector(&pairing, &claim("ai-mission", None)), None);
    assert_eq!(
        verify_injector(&pairing, &claim("ai-mission", Some("not-a-real-ticket"))),
        None
    );
}

#[test]
fn paired_node_accepts_a_valid_scoped_ticket() {
    use ados_protocol::ws_ticket::mint_scoped_ticket;
    let dir = tempfile::tempdir().unwrap();
    let pairing = dir.path().join("pairing.json");
    std::fs::write(&pairing, r#"{"paired": true, "api_key": "k-secret"}"#).unwrap();
    let ticket = mint_scoped_ticket(&pairing, &crsf_inject_scope("ai-mission"), 300)
        .expect("mint a ticket on a paired node");
    assert_eq!(
        verify_injector(&pairing, &claim("ai-mission", Some(&ticket))),
        Some("ai-mission".to_string())
    );
    // The same ticket does NOT verify for a DIFFERENT client id (scope-bound).
    assert_eq!(
        verify_injector(&pairing, &claim("other", Some(&ticket))),
        None
    );
}

// ── the feed and the hot-path cache ──────────────────────────────────────

fn feed_in(dir: &Path) -> GateFeed {
    GateFeed::with_paths(dir.join("pairing.json"), dir.join("pic-state.json"))
}

/// Write the PIC sidecar in the real schema `read_pic_view` parses.
fn write_pic(dir: &Path, holder: Option<&str>) {
    let body = match holder {
        Some(h) => format!(r#"{{"state": "claimed", "claimed_by": "{h}"}}"#),
        None => r#"{"state": "unclaimed"}"#.to_string(),
    };
    std::fs::write(dir.join("pic-state.json"), body).unwrap();
}

/// The gate reads only the feed: a sidecar change is invisible until the
/// feed refreshes, and then the verdict follows it.
#[test]
fn the_gate_follows_the_feed_not_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let feed = feed_in(dir.path());
    let mut gate = InjectorGateCache::new(&feed);
    let c = claim("ai-mission", None);

    write_pic(dir.path(), None);
    let t0 = Instant::now();
    feed.refresh(t0, SystemTime::now(), true);
    assert!(
        !gate.refused_at(&c, t0),
        "unclaimed PIC allows the injector"
    );

    // The operator grabs control; the file changes but the feed has not
    // refreshed, so the gate still sees the previous sample.
    write_pic(dir.path(), Some("hdmi-kiosk"));
    assert!(!gate.refused_at(&c, t0 + Duration::from_millis(10)));

    // The next refresh carries the grab, and the injector is refused.
    let t1 = t0 + PIC_REFRESH;
    feed.refresh(t1, SystemTime::now(), false);
    assert!(gate.refused_at(&c, t1), "the operator's grab refuses");
}

/// No sample yet, or a feed that stopped refreshing, is an arbiter that is
/// not reporting: refused.
#[test]
fn a_missing_or_stopped_feed_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let feed = feed_in(dir.path());
    let mut gate = InjectorGateCache::new(&feed);
    let c = claim("ai-mission", None);
    let t0 = Instant::now();
    assert!(gate.refused_at(&c, t0), "no sample yet");

    write_pic(dir.path(), None);
    feed.refresh(t0, SystemTime::now(), true);
    assert!(!gate.refused_at(&c, t0));
    assert!(
        gate.refused_at(&c, t0 + FEED_STALE_AFTER + Duration::from_millis(1)),
        "a stale feed must not keep granting the lane"
    );
}

/// Two injectors alternating used to thrash a single-slot cache, re-reading
/// the pairing file and recomputing the HMAC on every switch.
#[test]
fn alternating_claims_verify_at_most_once_each_per_ttl() {
    let dir = tempfile::tempdir().unwrap();
    let feed = feed_in(dir.path());
    write_pic(dir.path(), None);
    let t0 = Instant::now();
    feed.refresh(t0, SystemTime::now(), true);
    let mut gate = InjectorGateCache::new(&feed);
    let (a, b) = (claim("ai-a", None), claim("ai-b", None));
    for i in 0..100u32 {
        let now = t0 + Duration::from_millis(u64::from(i));
        gate.refused_at(if i % 2 == 0 { &a } else { &b }, now);
    }
    assert_eq!(gate.verifications(), 2, "one verify per distinct claim");
}

#[test]
fn the_verify_cache_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let feed = feed_in(dir.path());
    write_pic(dir.path(), None);
    let t0 = Instant::now();
    feed.refresh(t0, SystemTime::now(), true);
    let mut gate = InjectorGateCache::new(&feed);
    for i in 0..20 {
        gate.refused_at(&claim(&format!("c{i}"), None), t0);
    }
    assert!(gate.verify_cache.len() <= VERIFY_CACHE_CAP);
}

/// The verify is resolved per distinct claim: a paired node with no ticket
/// can never win the lane, whatever id it asserts.
#[test]
fn verify_is_resolved_per_distinct_claim() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("pairing.json"),
        r#"{"paired": true, "api_key": "k-secret"}"#,
    )
    .unwrap();
    write_pic(dir.path(), Some("ai-mission"));
    let feed = feed_in(dir.path());
    let now = Instant::now();
    feed.refresh(now, SystemTime::now(), true);
    let mut gate = InjectorGateCache::new(&feed);
    assert!(gate.refused_at(&claim("ai-mission", None), now));
    assert!(gate.refused_at(&claim("other", None), now));
    assert_eq!(gate.verifications(), 2);
}

/// A verification is not kept for the life of the lane: once the node is
/// paired after the injector connected, the same claim is checked against
/// the new key and loses the lane it can no longer prove.
#[test]
fn a_cached_verification_expires_and_is_rechecked() {
    let dir = tempfile::tempdir().unwrap();
    write_pic(dir.path(), Some("ai-mission"));
    let feed = feed_in(dir.path());
    let t0 = Instant::now();
    feed.refresh(t0, SystemTime::now(), true);
    let mut gate = InjectorGateCache::new(&feed);
    let c = claim("ai-mission", None);
    assert!(!gate.refused_at(&c, t0), "unpaired: the asserted id holds");

    std::fs::write(
        dir.path().join("pairing.json"),
        r#"{"paired": true, "api_key": "k-secret"}"#,
    )
    .unwrap();
    let later = t0 + VERIFY_CACHE_TTL + Duration::from_millis(1);
    feed.refresh(later, SystemTime::now(), true);
    assert!(
        gate.refused_at(&c, later),
        "an expired verification must be rechecked against the current key"
    );
}

#[test]
fn fresh_pic_view_is_absent_without_a_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let feed = feed_in(dir.path());
    let now = Instant::now();
    feed.refresh(now, SystemTime::now(), true);
    assert_eq!(feed.fresh_pic_view(now), None);
    write_pic(dir.path(), Some("op-a"));
    feed.refresh(now, SystemTime::now(), false);
    assert_eq!(
        feed.fresh_pic_view(now).and_then(|v| v.holder),
        Some("op-a".to_string())
    );
}
