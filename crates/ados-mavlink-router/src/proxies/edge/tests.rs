use super::*;

#[tokio::test(start_paused = true)]
async fn accept_errors_back_off_and_log_once_per_window() {
    let mut errors = AcceptErrors::default();
    let e = std::io::Error::other("emfile");
    let start = tokio::time::Instant::now();
    let mut logged = 0;
    for _ in 0..5 {
        if errors.on_error(5760, &e).await {
            logged += 1;
        }
    }
    assert_eq!(logged, 1, "one line per window, the rest counted");
    assert!(
        start.elapsed() >= ACCEPT_ERROR_BACKOFF * 5,
        "each error yields"
    );
}

#[test]
fn udp_refused_peer_one_warn() {
    let mut cache = UdpVerdicts::new(Duration::from_secs(2), 64);
    let peer: SocketAddr = "192.168.1.50:14550".parse().unwrap();
    let t0 = Instant::now();
    let mut lookups = 0;
    let mut warns = 0;
    for i in 0..100u64 {
        let now = t0 + Duration::from_millis(i);
        let got = cache.admit(peer, now, || {
            lookups += 1;
            (None, Access::Unauthorized)
        });
        assert_eq!(got.origin, None);
        if got.warn.is_some() {
            warns += 1;
        }
    }
    assert_eq!(warns, 1);
    assert_eq!(lookups, 1, "one route lookup per TTL");
    // After the TTL the verdict is re-decided; after the log window the
    // warning carries the suppressed count.
    let later = t0 + UDP_REFUSAL_LOG_EVERY;
    let got = cache.admit(peer, later, || (None, Access::Unauthorized));
    assert_eq!(got.warn, Some(99));
}

#[test]
fn the_verdict_cache_is_bounded() {
    let mut cache = UdpVerdicts::new(Duration::from_secs(2), 4);
    let now = Instant::now();
    for p in 0..10u16 {
        let peer: SocketAddr = format!("192.168.1.50:{}", 1000 + p).parse().unwrap();
        cache.admit(peer, now + Duration::from_millis(u64::from(p)), || {
            (Some(ClientOrigin::Trusted), Access::Accept)
        });
    }
    assert_eq!(cache.entries.len(), 4);
}

#[test]
fn the_peer_generation_moves_only_on_a_change() {
    let peers = UdpPeers::default();
    let a: SocketAddr = "127.0.0.1:1".parse().unwrap();
    let now = Instant::now();
    peers.touch(a, now, 64);
    let g = peers.generation();
    peers.touch(a, now, 64);
    assert_eq!(peers.generation(), g, "a refresh is not a change");
    peers.evict(now + super::super::UDP_PEER_TTL + Duration::from_secs(1));
    assert_ne!(peers.generation(), g, "an eviction is");
    let mut targets = vec![a];
    peers.fill(&mut targets);
    assert!(targets.is_empty());
}
