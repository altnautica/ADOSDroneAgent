use std::sync::Arc;

use ados_time::{wire::Tick, SimFeed};

use super::*;

/// A clock on a feed the test drives, and that feed.
pub(crate) fn sim_clock() -> (RouterClock, Arc<SimFeed>) {
    let feed = SimFeed::new();
    (RouterClock::new(Clock::Sim(feed.clone())), feed)
}

/// Move the feed to `sim_ms` (sequence `seq`), received now.
pub(crate) fn advance(feed: &SimFeed, seq: u32, sim_ms: u64) {
    let tick = Tick {
        flags: 0,
        seq,
        sim_ns: sim_ms * 1_000_000,
        grant_ns: (sim_ms + 20) * 1_000_000,
        speed: 1.0,
    };
    assert!(feed.ingest(&tick, std::time::Instant::now()));
}

#[test]
fn sim_time_moves_only_with_the_feed() {
    let (clock, feed) = sim_clock();
    assert_eq!(clock.source(), "sim");
    advance(&feed, 1, 1_000);
    let at = clock.now();
    assert_eq!(clock.now(), at, "no tick, no time");
    advance(&feed, 2, 7_500);
    assert_eq!(clock.since(at), Duration::from_millis(6_500));
    assert_eq!(clock.now_ns(), 7_500_000_000);
}

#[test]
fn wall_time_is_the_real_monotonic_clock() {
    let clock = RouterClock::default();
    assert_eq!(clock.source(), "wall");
    let before = std::time::Instant::now();
    let now = clock.now();
    assert!(now >= before && now <= std::time::Instant::now());
}
