use std::future::Future;
use std::time::{Duration, Instant};

use super::wire::Tick;
use super::*;

fn tick(seq: u32, sim_ms: u64) -> Tick {
    Tick {
        flags: 0,
        seq,
        sim_ns: sim_ms * 1_000_000,
        grant_ns: (sim_ms + 20) * 1_000_000,
        speed: 1.0,
    }
}

async fn not_ready<F: Future>(f: F) -> bool {
    tokio::time::timeout(Duration::from_millis(50), f)
        .await
        .is_err()
}

#[tokio::test(start_paused = true)]
async fn sim_interval_ticks_on_feed() {
    let feed = SimFeed::new();
    let clock = Clock::Sim(feed.clone());
    let mut iv = clock.interval(Duration::from_millis(20));
    let at = Instant::now();
    assert!(
        not_ready(iv.tick()).await,
        "no tick before the first feed time"
    );
    feed.ingest(&tick(1, 0), at);
    assert_eq!(iv.tick().await, 0);
    feed.ingest(&tick(2, 10), at);
    assert!(not_ready(iv.tick()).await);
    feed.ingest(&tick(3, 20), at);
    assert_eq!(iv.tick().await, 20);
    // A jump skips the missed ticks.
    feed.ingest(&tick(4, 75), at);
    assert_eq!(iv.tick().await, 75);
    assert!(not_ready(iv.tick()).await);
    feed.ingest(&tick(5, 80), at);
    assert_eq!(iv.tick().await, 80);
    assert_eq!(clock.now_ms(), 80);
    assert_eq!(clock.source(), "sim");

    feed.ingest(&tick(6, 100), at);
    clock.sleep_until_ms(100).await;
    assert!(not_ready(clock.sleep_until_ms(101)).await);
}

#[tokio::test(start_paused = true)]
async fn sim_feed_stale_pauses() {
    let feed = SimFeed::new();
    let clock = Clock::Sim(feed.clone());
    let mut iv = clock.interval(Duration::from_millis(20));
    let t0 = Instant::now();
    feed.ingest(&tick(1, 0), t0);
    assert_eq!(iv.tick().await, 0);
    feed.ingest(&tick(2, 20), t0);
    assert!(!feed.check_stale(t0 + Duration::from_millis(400)));
    assert!(feed.check_stale(t0 + Duration::from_millis(600)));
    assert!(clock.is_stale());
    // Due, but the feed is stale: the timer waits, and time stays sim time.
    assert!(not_ready(iv.tick()).await);
    assert!(not_ready(clock.sleep_until_ms(20)).await);
    assert_eq!(clock.now_ms(), 20);
    feed.ingest(&tick(3, 30), t0 + Duration::from_millis(700));
    assert!(!clock.is_stale());
    assert_eq!(iv.tick().await, 30);
}

#[test]
fn out_of_order_seq_ignored() {
    let feed = SimFeed::new();
    let at = Instant::now();
    assert!(feed.ingest(&tick(5, 50), at));
    assert!(!feed.ingest(&tick(4, 40), at));
    assert!(!feed.ingest(&tick(5, 55), at));
    assert_eq!(feed.sim_ns(), Some(50_000_000));
    assert!(feed.ingest(&tick(6, 60), at));
    // Time never goes backwards, even on a newer seq.
    assert!(!feed.ingest(&tick(7, 59), at));
    // seq wraps.
    let wrap = SimFeed::new();
    assert!(wrap.ingest(&tick(u32::MAX, 1), at));
    assert!(wrap.ingest(&tick(0, 2), at));
    assert!(!wrap.ingest(&tick(u32::MAX, 3), at));
}

#[tokio::test(start_paused = true)]
async fn wall_mode_matches_tokio_interval() {
    let clock = Clock::wall();
    assert_eq!(clock.source(), "wall");
    assert_eq!(clock.sim_ns(), None);
    let period = Duration::from_millis(10);

    let start = tokio::time::Instant::now();
    let mut ours = clock.interval(period);
    for _ in 0..10 {
        ours.tick().await;
    }
    let ours_took = start.elapsed();

    let start = tokio::time::Instant::now();
    let mut theirs = tokio::time::interval(period);
    theirs.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    for _ in 0..10 {
        theirs.tick().await;
    }
    assert_eq!(ours_took, start.elapsed());
    assert_eq!(ours_took, Duration::from_millis(90));
}

#[tokio::test]
async fn a_relay_follower_reads_whole_ticks_and_reconnects() {
    let dir = std::env::temp_dir().join(format!("simclock-relay-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("relay.sock");
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let clock = Clock::Sim(SimFeed::follow_relay(path.clone()));
    for (seq, ms) in [(1u32, 40u64), (2, 60)] {
        let (mut s, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
            .await
            .unwrap()
            .unwrap();
        use tokio::io::AsyncWriteExt;
        // Split one tick across two writes: the follower reassembles it.
        let b = tick(seq, ms).encode();
        s.write_all(&b[..10]).await.unwrap();
        s.write_all(&b[10..]).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), clock.sleep_until_ms(ms))
            .await
            .unwrap();
        assert_eq!(clock.now_ms(), ms);
        // Dropping the stream: the follower reconnects for the next one.
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn no_env_is_wall() {
    if std::env::var_os(CLOCK_FEED_ENV).is_none() {
        assert_eq!(Clock::from_env().source(), "wall");
    }
}

#[tokio::test(start_paused = true)]
async fn next_due_follows_the_sim_interval() {
    let feed = SimFeed::new();
    let clock = Clock::Sim(feed.clone());
    let mut iv = clock.interval(Duration::from_millis(20));
    assert_eq!(iv.next_due_ns(), None);
    feed.ingest(&tick(1, 0), Instant::now());
    iv.tick().await;
    assert_eq!(iv.next_due_ns(), Some(20_000_000));
    feed.ingest(&tick(2, 45), Instant::now());
    iv.tick().await;
    assert_eq!(iv.next_due_ns(), Some(60_000_000));
    assert_eq!(
        Clock::wall()
            .interval(Duration::from_millis(5))
            .next_due_ns(),
        None
    );
}

/// One `DONE` frame's `done_ns` from `s`.
async fn read_done(s: &mut tokio::net::UnixStream) -> u64 {
    use tokio::io::AsyncReadExt;
    let mut b = [0u8; wire::DONE_LEN];
    tokio::time::timeout(Duration::from_secs(3), s.read_exact(&mut b))
        .await
        .unwrap()
        .unwrap();
    wire::Done::decode(&b).unwrap().done_ns
}

#[tokio::test]
async fn a_participant_writes_its_latest_done_and_reregisters() {
    use tokio::io::AsyncReadExt;
    let dir = std::env::temp_dir().join(format!("simclock-part-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = participants_socket_beside(&dir.join("relay.sock"));
    assert_eq!(path, dir.join(PARTICIPANTS_SOCKET));
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let part = Participant::connect(path.clone());
    part.done(40_000_000);
    part.done(30_000_000);
    assert_eq!(part.reported(), Some(40_000_000));
    let (mut s, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        read_done(&mut s).await,
        40_000_000,
        "the latest report on connect"
    );
    part.done(60_000_000);
    assert_eq!(read_done(&mut s).await, 60_000_000);
    // The barrier restarts: the participant registers again with its latest report.
    drop(s);
    part.done(80_000_000);
    let (mut s, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read_done(&mut s).await, 80_000_000);
    // Dropping the participant unregisters it: the connection closes.
    drop(part);
    let mut b = [0u8; 1];
    let n = tokio::time::timeout(Duration::from_secs(3), s.read(&mut b))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(n, 0);
    let _ = std::fs::remove_dir_all(&dir);
}
