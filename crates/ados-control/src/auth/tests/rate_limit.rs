//! The per-caller request budget.

use super::*;

fn peer(ip: &str) -> PeerKey {
    PeerKey::of(Some(ip.parse().unwrap()))
}

#[test]
fn rate_limiter_admits_up_to_capacity_then_rejects() {
    let limiter = RateLimiter::new(3, Duration::from_secs(60));
    let a = peer("192.168.1.50");
    assert!(limiter.check(a));
    assert!(limiter.check(a));
    assert!(limiter.check(a));
    // Fourth in the same window is rejected.
    assert!(!limiter.check(a));
}

/// One host exhausting its budget leaves every other caller's untouched: a
/// single shared bucket let any LAN host 429 the paired operator.
#[test]
fn one_caller_exhausting_its_budget_does_not_starve_another() {
    let limiter = RateLimiter::new(2, Duration::from_secs(60));
    let flooder = peer("192.168.1.66");
    while limiter.check(flooder) {}
    assert!(
        limiter.check(peer("192.168.1.50")),
        "the operator is served"
    );
    // Addresses within one IPv6 /64 are one caller.
    let v6 = peer("2001:db8:1:2::5");
    assert!(limiter.check(v6));
    assert!(limiter.check(peer("2001:db8:1:2::6")));
    assert!(!limiter.check(peer("2001:db8:1:2:ffff::1")));
}

#[test]
fn rate_limiter_refills_after_the_window() {
    let limiter = RateLimiter::new(1, Duration::from_millis(20));
    let a = peer("192.168.1.50");
    assert!(limiter.check(a));
    assert!(!limiter.check(a));
    std::thread::sleep(Duration::from_millis(30));
    assert!(limiter.check(a), "the window refilled");
}

/// Link-local and unique-local callers share a /64 with every host on the
/// LAN, so each is its own caller; a global /64 still collapses.
#[test]
fn link_local_and_ula_peers_each_get_their_own_budget() {
    let limiter = RateLimiter::new(60, Duration::from_secs(60));
    for pair in [
        ("fe80::1", "fe80::2"),
        ("febf::1", "febf::2"),
        ("fd12:3456::1", "fd12:3456::2"),
    ] {
        let (a, b) = (peer(pair.0), peer(pair.1));
        assert_ne!(a, b, "{pair:?}");
        while limiter.check(a) {}
        assert!(limiter.check(b), "{} keeps its budget", pair.1);
    }
    assert_eq!(peer("2001:db8:1:2::10"), peer("2001:db8:1:2::20"));
}
