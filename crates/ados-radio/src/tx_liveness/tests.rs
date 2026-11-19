//! Liveness window and watcher tests.
use super::*;

/// A real `wfb_tx` stats line (wfb-ng 24.08 `tx.cpp` format string).
const LINE: &str = "1750000000000\tPKT\t0:12:1480:12:1608:0:0";

#[test]
fn a_real_pkt_line_yields_the_ingress_and_injection_bytes() {
    let pkt = parse_tx_pkt(LINE).expect("the shipped format must parse");
    assert_eq!(
        pkt,
        TxPkt {
            bytes_in: 1480,
            bytes_injected: 1608,
            packets_dropped: 0,
        }
    );
}

#[test]
fn non_pkt_lines_are_ignored_rather_than_treated_as_a_fault() {
    // `TX_ANT` shares the stream, and upstream may add lines. Reading any of
    // them as a fault would restart the node that carries command authority
    // on a cosmetic format change.
    assert!(parse_tx_pkt("1750000000000\tTX_ANT\t1\t12:0:100:200:300").is_none());
    assert!(parse_tx_pkt("").is_none());
    assert!(parse_tx_pkt("garbage").is_none());
    assert!(parse_tx_pkt("1750000000000\tPKT\t0:12").is_none());
    assert!(parse_tx_pkt("1750000000000\tPKT\t0:12:x:12:1608:0:0").is_none());
}

fn pkt(bytes_in: u64, bytes_injected: u64) -> TxPkt {
    TxPkt {
        bytes_in,
        bytes_injected,
        packets_dropped: 0,
    }
}

#[tokio::test(start_paused = true)]
async fn a_transmitter_injecting_what_it_is_given_is_healthy() {
    let t0 = Instant::now();
    let mut w = TxLivenessWindow::new(t0, Duration::from_secs(30));
    let mut first = None;
    let mut ticks_before_verdict = 0u64;
    for _ in 1..=31u64 {
        tokio::time::advance(Duration::from_secs(1)).await;
        w.observe(pkt(1480, 1608), Instant::now());
        match w.evaluate(Instant::now()) {
            None => ticks_before_verdict += 1,
            Some(v) => {
                first = Some(v);
                break;
            }
        }
    }
    assert_eq!(first, Some(TxVerdict::Healthy));
    // And it withheld judgement until the window had actually filled, so a
    // fast poll cadence can never restart a transmitter on a partial view.
    assert_eq!(ticks_before_verdict, 29);
}

#[tokio::test(start_paused = true)]
async fn a_transmitter_with_nothing_to_send_is_idle_not_stalled() {
    // The exemption that keeps a working-but-quiet transmitter alive. Before
    // it, a 30 s lull would have killpg'd the uplink and restarted the whole
    // receive generation for no fault at all.
    let mut w = TxLivenessWindow::new(Instant::now(), Duration::from_secs(30));
    for _ in 0..40 {
        tokio::time::advance(Duration::from_secs(1)).await;
        w.observe(pkt(0, 0), Instant::now());
        if let Some(v) = w.evaluate(Instant::now()) {
            assert_eq!(v, TxVerdict::Idle);
            assert!(!v.is_stall());
        }
    }
}

#[tokio::test(start_paused = true)]
async fn ingress_with_no_injection_for_the_window_is_a_stall() {
    let mut w = TxLivenessWindow::new(Instant::now(), Duration::from_secs(30));
    let mut fired = None;
    for _ in 0..40 {
        tokio::time::advance(Duration::from_secs(1)).await;
        w.observe(pkt(1480, 0), Instant::now());
        if let Some(v) = w.evaluate(Instant::now()) {
            fired = Some(v);
            break;
        }
    }
    assert_eq!(fired, Some(TxVerdict::StalledNotInjecting));
}

#[tokio::test(start_paused = true)]
async fn no_stats_line_for_the_window_is_a_stall_even_with_no_traffic() {
    // `wfb_tx` prints every second regardless of traffic, so silence on the
    // stream means the main loop stopped — the failure that used to be
    // completely invisible because nothing read the stream at all.
    let mut w = TxLivenessWindow::new(Instant::now(), Duration::from_secs(30));
    tokio::time::advance(Duration::from_secs(31)).await;
    assert_eq!(
        w.evaluate(Instant::now()),
        Some(TxVerdict::StalledSilent),
        "line silence must fire without needing any ingress"
    );
}

#[tokio::test(start_paused = true)]
async fn the_watcher_fires_on_a_flat_injection_counter() {
    // End to end over the real reader: a transmitter that keeps printing and
    // keeps being fed but injects nothing must resolve the watcher, which is
    // what ends the generation and respawns the chain.
    let (mut writer, reader) = tokio::io::duplex(4096);
    let feeder = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        for _ in 0..60 {
            // Ingress advancing, injection flat: the dead-radio shape.
            let _ = writer
                .write_all(b"1750000000000\tPKT\t0:12:1480:0:0:0:0\n")
                .await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
    let verdict = tokio::time::timeout(
        Duration::from_secs(300),
        watch_tx_liveness(
            "aux_tx",
            reader,
            Duration::from_secs(5),
            Duration::from_secs(30),
            running(),
        ),
    )
    .await
    .expect("the watcher must resolve, not hang");
    assert_eq!(verdict, TxVerdict::StalledNotInjecting);
    feeder.abort();
}

#[tokio::test(start_paused = true)]
async fn the_watcher_fires_when_the_transmitter_stops_printing() {
    // The process died or wedged: the pipe closes (or simply goes quiet) and
    // the watcher must conclude rather than park on the stream forever.
    let (writer, reader) = tokio::io::duplex(4096);
    drop(writer);
    let verdict = tokio::time::timeout(
        Duration::from_secs(300),
        watch_tx_liveness(
            "tx_control",
            reader,
            Duration::from_secs(5),
            Duration::from_secs(30),
            running(),
        ),
    )
    .await
    .expect("the watcher must resolve on a closed stream");
    assert_eq!(verdict, TxVerdict::StalledSilent);
}

#[tokio::test(start_paused = true)]
async fn the_watcher_does_not_fire_on_a_healthy_transmitter() {
    // The regression that matters most: a false fire restarts the uplink, so
    // a healthy stream must be left alone across several windows.
    let (mut writer, reader) = tokio::io::duplex(4096);
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        loop {
            let _ = writer.write_all(LINE.as_bytes()).await;
            let _ = writer.write_all(b"\n").await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
    let outcome = tokio::time::timeout(
        Duration::from_secs(120),
        watch_tx_liveness(
            "tx_control",
            reader,
            Duration::from_secs(5),
            Duration::from_secs(30),
            running(),
        ),
    )
    .await;
    assert!(
        outcome.is_err(),
        "a healthy transmitter must never trigger a restart, got {outcome:?}"
    );
}

/// The flag of a transmitter nobody stopped. The sender is leaked so the
/// receiver keeps its last value for the test's life.
fn running() -> tokio::sync::watch::Receiver<bool> {
    let (tx, rx) = tokio::sync::watch::channel(false);
    std::mem::forget(tx);
    rx
}

#[tokio::test(start_paused = true)]
async fn a_deliberately_stopped_transmitter_reads_standby_not_a_stall() {
    // A ground station standing by stops both transmitters while a phone
    // holds the uplink: their silence must never read as a dead radio.
    let mut w = TxLivenessWindow::new(Instant::now(), Duration::from_secs(30));
    tokio::time::advance(Duration::from_secs(120)).await;
    let v = w.evaluate_stopped(Instant::now(), true);
    assert_eq!(v, Some(TxVerdict::Standby));
    assert!(!TxVerdict::Standby.is_stall());
    assert_eq!(TxVerdict::Standby.as_str(), "standby");
    // Started again, the window judges the new run from now, not from the
    // two minutes of deliberate silence.
    tokio::time::advance(Duration::from_secs(5)).await;
    assert_eq!(w.evaluate_stopped(Instant::now(), false), None);
}

#[tokio::test(start_paused = true)]
async fn the_watcher_does_not_fire_when_the_stop_was_deliberate() {
    // The stop raises the flag, then terminates the process: the closed
    // pipe that follows must not end the receive generation.
    let (flag, stopped) = tokio::sync::watch::channel(false);
    let (writer, reader) = tokio::io::duplex(4096);
    flag.send(true).unwrap();
    drop(writer);
    let outcome = tokio::time::timeout(
        Duration::from_secs(300),
        watch_tx_liveness(
            "aux_tx",
            reader,
            Duration::from_secs(5),
            Duration::from_secs(30),
            stopped,
        ),
    )
    .await;
    assert!(outcome.is_err(), "a standby transmitter fired: {outcome:?}");
}
