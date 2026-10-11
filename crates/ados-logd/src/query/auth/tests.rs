use super::*;
use std::io::Write;

fn write_pairing(dir: &std::path::Path, body: &str) -> PathBuf {
    let path = dir.join("pairing.json");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    path
}

#[test]
fn unpaired_serves_first_boot_reach_only() {
    let dir = tempfile::tempdir().unwrap();
    // No pairing file at all → unpaired.
    let state = PairingState::with_path(dir.path().join("absent.json"));
    assert_eq!(state.current(), Pairing::Unpaired);
    assert!(state.authorize("/v1/query", CallerClass::OnBox, None));
    assert!(state.authorize("/v1/query", CallerClass::Lifeline, None));
    // The history of an unclaimed node is not served to the wider LAN
    // (the agent's HTTP edge wants a PIN session there) or to anything
    // remote, key or not.
    assert!(!state.authorize("/v1/query", CallerClass::OperatorLan, None));
    assert!(!state.authorize("/v1/query", CallerClass::Remote, Some("anything")));
}

#[test]
fn paired_requires_the_exact_key_from_every_caller() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_pairing(dir.path(), r#"{"paired": true, "api_key": "ados_secret"}"#);
    let state = PairingState::with_path(path);
    assert_eq!(state.current(), Pairing::Paired("ados_secret".to_string()));
    for caller in [
        CallerClass::OnBox,
        CallerClass::Lifeline,
        CallerClass::OperatorLan,
        CallerClass::Remote,
    ] {
        assert!(state.authorize("/v1/query", caller, Some("ados_secret")));
        assert!(!state.authorize("/v1/query", caller, Some("ados_secre1")));
        assert!(!state.authorize("/v1/query", caller, Some("ados")));
        assert!(!state.authorize("/v1/query", caller, None));
    }
}

#[test]
fn public_paths_are_open_even_when_paired() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_pairing(dir.path(), r#"{"paired": true, "api_key": "k"}"#);
    let state = PairingState::with_path(path);
    assert!(state.authorize("/v1/healthz", CallerClass::Remote, None));
    assert!(state.authorize("/v1/openapi.json", CallerClass::Remote, None));
}

/// A pairing file that exists but cannot be trusted may be a paired node's
/// record on a failing card. Reading it as unpaired would open the store.
#[test]
fn an_unreadable_pairing_file_fails_closed() {
    for body in ["this is not json", r#"{"paired": true, "api_key": ""}"#] {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pairing(dir.path(), body);
        let state = PairingState::with_path(path);
        assert_eq!(state.current(), Pairing::Unreadable, "{body}");
        assert!(!state.authorize("/v1/query", CallerClass::OnBox, None));
        assert!(!state.authorize("/v1/query", CallerClass::Lifeline, Some("k")));
    }
}

#[test]
fn rate_limiter_admits_up_to_capacity_then_rejects() {
    let limiter = RateLimiter::new(3, Duration::from_secs(60));
    assert!(limiter.check());
    assert!(limiter.check());
    assert!(limiter.check());
    // Fourth in the same window is rejected.
    assert!(!limiter.check());
}

#[test]
fn rate_limiter_refills_after_the_window() {
    let limiter = RateLimiter::new(1, Duration::from_millis(20));
    assert!(limiter.check());
    assert!(!limiter.check());
    std::thread::sleep(Duration::from_millis(30));
    assert!(limiter.check(), "the window refilled");
}

fn ip(n: u32) -> IpAddr {
    IpAddr::V4(std::net::Ipv4Addr::from(0x0a00_0000 + n))
}

#[test]
fn per_ip_map_never_exceeds_256_entries() {
    let limiter = PeerRateLimiter::new(1, Duration::from_secs(3600), MAX_TRACKED_PEERS);
    assert_eq!(MAX_TRACKED_PEERS, 256);
    for n in 0..1_000 {
        limiter.check(ip(n));
        assert!(
            limiter.tracked_peers() <= MAX_TRACKED_PEERS,
            "map grew past the cap at peer {n}"
        );
    }
    assert_eq!(limiter.tracked_peers(), MAX_TRACKED_PEERS);
}

#[test]
fn the_least_recently_seen_peer_is_evicted_first() {
    let limiter = PeerRateLimiter::new(1, Duration::from_secs(3600), 3);
    for n in 0..3 {
        assert!(limiter.check(ip(n)), "first request from a peer has budget");
    }
    // Peer 0 is seen again (and is now out of budget), so peer 1 is the LRU.
    assert!(!limiter.check(ip(0)));
    // A fourth peer evicts peer 1, not peer 0.
    assert!(limiter.check(ip(3)));
    assert_eq!(limiter.tracked_peers(), 3);
    assert!(!limiter.check(ip(0)), "peer 0 kept its spent bucket");
    // Peer 1 was evicted; re-admitting it evicts the LRU again (peer 2).
    assert!(limiter.check(ip(1)), "peer 1 was evicted and starts fresh");
    assert_eq!(limiter.tracked_peers(), 3);
}

#[test]
fn peers_have_independent_buckets() {
    let limiter = PeerRateLimiter::new(2, Duration::from_secs(3600), 8);
    assert!(limiter.check(ip(1)));
    assert!(limiter.check(ip(1)));
    assert!(!limiter.check(ip(1)), "peer 1 spent its budget");
    assert!(limiter.check(ip(2)), "peer 2 is unaffected");
}
