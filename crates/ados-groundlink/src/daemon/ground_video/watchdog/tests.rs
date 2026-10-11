use super::*;

fn run(seq: &[(Option<u64>, bool)]) -> Vec<Verdict> {
    let mut wd = StallWatchdog::new();
    let t0 = Instant::now();
    seq.iter()
        .enumerate()
        .map(|(i, (b, rx))| wd.observe(*b, *rx, t0 + POLL_INTERVAL * i as u32))
        .collect()
}

#[test]
fn watchdog_restarts_on_stall_with_rx() {
    // Grows once, then flat for 12 s with RX active.
    let mut seq = vec![(Some(100), true), (Some(200), true)];
    seq.extend(std::iter::repeat_n((Some(200), true), 6));
    let v = run(&seq);
    assert!(v[..7].iter().all(|v| *v == Verdict::Healthy));
    assert_eq!(v[7], Verdict::Restart);

    // An unreachable API while RX flows is also a stall.
    let mut seq = vec![(Some(5), true)];
    seq.extend(std::iter::repeat_n((None, true), 6));
    assert_eq!(*run(&seq).last().unwrap(), Verdict::Restart);
}

#[test]
fn idle_without_rx_no_restart() {
    let seq: Vec<_> = std::iter::repeat_n((Some(0), false), 30).collect();
    assert!(run(&seq).iter().all(|v| *v == Verdict::Healthy));

    // RX idle partway resets the window.
    let mut seq = vec![(Some(10), true)];
    seq.extend(std::iter::repeat_n((Some(10), true), 4));
    seq.push((Some(10), false));
    seq.extend(std::iter::repeat_n((Some(10), true), 5));
    assert!(run(&seq).iter().all(|v| *v == Verdict::Healthy));
}

#[test]
fn rx_active_reads_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("wfb-stats.json");
    assert!(!rx_active(&p));
    std::fs::write(&p, br#"{"packets_received": 0}"#).unwrap();
    assert!(!rx_active(&p));
    std::fs::write(&p, br#"{"packets_received": 42}"#).unwrap();
    assert!(rx_active(&p));
}
