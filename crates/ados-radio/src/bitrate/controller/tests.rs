use super::*;
use ados_protocol::link_feedback::{LinkFeedback, LinkFeedbackSidecar};
use std::sync::atomic::AtomicBool;

fn peer_report(loss: f64, at_ms: u64) -> LinkFeedbackSidecar {
    LinkFeedbackSidecar::stamped(
        &LinkFeedback {
            loss_percent: loss,
            rssi_dbm: -36.0,
            snr_db: 12.4,
            packets_received: 485,
            fec_failed: 25,
            bitrate_kbps: 2242,
            has_measurement: true,
            target_slot: 1,
        },
        at_ms,
    )
}

#[test]
fn the_peers_report_drives_the_ladders() {
    let peer = peer_report(24.29, 10_000);
    let s = resolve_sample(Some(&peer), 10_500);
    assert_eq!(s.source, SampleSource::Peer);
    assert!(s.has_sample(), "the ladder must be able to act");
    assert!((s.loss_percent - 24.29).abs() < 0.01);
}

#[test]
fn a_stale_peer_report_is_not_a_sample() {
    // Feedback usually stops because the link got WORSE. Holding the last
    // good report would keep the rate high exactly when it should fall.
    let peer = peer_report(2.0, 10_000);
    let s = resolve_sample(Some(&peer), 99_000);
    assert_eq!(s.source, SampleSource::None);
    assert!(!s.has_sample());
}

#[test]
fn a_peer_that_heard_nothing_is_not_a_clean_link() {
    let deaf = LinkFeedbackSidecar::stamped(
        &LinkFeedback {
            loss_percent: 0.0,
            rssi_dbm: -100.0,
            snr_db: 0.0,
            packets_received: 0,
            fec_failed: 0,
            bitrate_kbps: 0,
            has_measurement: false,
            target_slot: 1,
        },
        10_000,
    );
    let s = resolve_sample(Some(&deaf), 10_100);
    assert_eq!(
        s.source,
        SampleSource::None,
        "a deaf receiver reporting 0% loss must not read as a perfect link"
    );
}

#[test]
fn no_peer_report_at_all_leaves_the_node_without_a_sample() {
    let s = resolve_sample(None, 10_000);
    assert_eq!(s.source, SampleSource::None);
    assert!(
        !s.has_sample(),
        "falls through to the no-sample path, not to a fabricated sample"
    );
}

#[test]
fn shared_enable_handle_is_read_live_not_captured() {
    // The controller must read the shared flag, not snapshot it at
    // construction: flipping the handle AFTER the controller is built changes
    // what the next tick would do. Constructing the controller over a shared
    // handle and then flipping it proves the wiring without running a tick
    // (which would fork wfb_tx).
    let flag: EnabledHandle = Arc::new(AtomicBool::new(false));
    let ctrl = BitrateController::with_tiers_shared(
        DEFAULT_TIERS.to_vec(),
        flag.clone(),
        DEFAULT_TICK_INTERVAL,
    );
    // The controller and the command-socket side hold the SAME atomic.
    assert!(!ctrl.enabled.load(Ordering::Relaxed));
    flag.store(true, Ordering::Relaxed);
    assert!(ctrl.enabled.load(Ordering::Relaxed));
}

/// The controller seeds its MCS ladder from the configured cap and the rung
/// `wfb_tx` was actually spawned on, not from the ladder's own defaults.
#[test]
fn controller_seeds_the_mcs_ladder_from_config() {
    let ctrl = BitrateController::new(Arc::new(AtomicBool::new(true)), 5, 3);
    assert_eq!(ctrl.mcs.cap(), 5);
    assert_eq!(ctrl.mcs.current(), 3);
    // An out-of-range cap is clamped rather than trusted.
    let capped = BitrateController::new(Arc::new(AtomicBool::new(true)), 7, 1);
    assert_eq!(capped.mcs.cap(), mcs_ladder::LADDER_MAX_MCS);
}

/// The controller and `ados-video` agree at boot (both "no ceiling"), so the
/// first tick must not issue a spurious publish.
#[test]
fn no_ceiling_publish_when_both_sides_start_unclamped() {
    assert!(!ceiling_needs_apply(None, None, None));
    assert!(!ceiling_needs_apply(Some(None), None, None));
}

/// A rung change publishes; a repeat of the same rung does not.
#[test]
fn ceiling_publishes_on_change_and_is_idempotent() {
    // Video reports 4000 live, ladder wants 1200 -> publish.
    assert!(ceiling_needs_apply(
        Some(Some(4000)),
        Some(4000),
        Some(1200)
    ));
    // Already 1200 live -> nothing to do, even mid-rung-change.
    assert!(!ceiling_needs_apply(
        Some(Some(1200)),
        Some(4000),
        Some(1200)
    ));
}

/// The self-heal: `ados-video` restarted and came back with the ceiling
/// cleared while this controller still believes it published 1200. The
/// published truth wins, so the next tick re-asserts.
#[test]
fn ceiling_reasserts_after_ados_video_restart_cleared_it() {
    assert!(ceiling_needs_apply(Some(None), Some(1200), Some(1200)));
}

/// With `ados-video` down (no sidecar) the controller falls back to its own
/// last publish, so a dead service costs one attempt per rung change rather
/// than one attempt every tick.
#[test]
fn ceiling_does_not_retry_every_tick_while_video_is_down() {
    // Same intent as last publish: hold.
    assert!(!ceiling_needs_apply(None, Some(1200), Some(1200)));
    // Intent changed: try once.
    assert!(ceiling_needs_apply(None, Some(1200), Some(2000)));
}

/// The gap the existing "does not retry every tick" test could not cover.
///
/// It only exercised `asserted == Some(_)`, i.e. a controller that had
/// published successfully at least once. A node with NO encoder never gets
/// there: `asserted` stays `None`, so `ceiling_needs_apply(None, None,
/// Some(x))` is true on every tick from boot, and the failed publish it
/// triggers logged a warning once a second for the life of the process.
#[test]
fn a_node_that_has_never_published_still_wants_to_apply_every_tick() {
    // This is the condition the backoff exists to survive; it is correct
    // (the ceiling really has not been delivered) and it never clears on
    // its own, which is exactly why the ATTEMPT has to be rate-limited
    // rather than the check changed.
    assert!(ceiling_needs_apply(None, None, Some(4000)));
}

#[test]
fn a_node_with_no_encoder_retries_on_a_fixed_cadence_not_every_tick() {
    // Five minutes of 1 Hz ticks against an encoder that never answers.
    // Unthrottled this was 300 attempts and 300 warnings; on the fixed
    // cadence it is one attempt per retry interval, never tapering off.
    let mut retry = CeilingRetry::default();
    let t0 = Instant::now();
    let mut attempts = 0u32;
    for tick in 0..300u64 {
        let now = t0 + Duration::from_secs(tick);
        if retry.should_attempt(Some(4000), now) {
            attempts += 1;
            retry.record_failure(Some(4000), now);
        }
    }
    assert_eq!(
        attempts,
        (300 / CEILING_RETRY_INTERVAL.as_secs()) as u32,
        "one attempt per fixed retry interval"
    );
}

#[test]
fn the_failure_is_still_reported_just_not_every_second() {
    // The signal must survive the rate limit. Loud on the first failure,
    // then periodically with the running total, never silent.
    assert!(
        CeilingRetry::should_warn(1),
        "the first failure must be loud"
    );
    assert!(!CeilingRetry::should_warn(2));
    assert!(!CeilingRetry::should_warn(29));
    assert!(
        CeilingRetry::should_warn(CEILING_LOG_EVERY),
        "a persistent failure must keep surfacing"
    );
    assert!(CeilingRetry::should_warn(CEILING_LOG_EVERY * 4));
}

#[test]
fn a_rung_change_is_attempted_at_once_despite_the_backoff() {
    // The backoff must not delay NEW information. A rung step is something
    // the encoder has not been told yet, so it goes out immediately.
    let mut retry = CeilingRetry::default();
    let t0 = Instant::now();
    retry.record_failure(Some(4000), t0);
    assert!(
        !retry.should_attempt(Some(4000), t0),
        "the same intent waits for the backoff"
    );
    assert!(
        retry.should_attempt(Some(2000), t0),
        "a rung change must not wait"
    );
    // Disarming the ladder clears the clamp, which is also new intent.
    assert!(retry.should_attempt(None, t0));
}

#[test]
fn a_successful_publish_clears_the_backoff_and_reports_the_outage() {
    let mut retry = CeilingRetry::default();
    let t0 = Instant::now();
    retry.record_failure(Some(4000), t0);
    retry.record_failure(Some(4000), t0);
    assert_eq!(
        retry.record_success(Some(4000)),
        2,
        "recovery must say how long it had been failing"
    );
    assert!(retry.should_attempt(Some(4000), t0), "backoff is cleared");
    assert_eq!(retry.failures, 0);
}

#[test]
fn the_retry_gap_stays_fixed_however_long_the_outage() {
    // A long outage must not stretch the gap: an encoder that comes back
    // after forty failures gets its ceiling one interval later, not minutes.
    let mut retry = CeilingRetry::default();
    let t0 = Instant::now();
    for _ in 0..40 {
        retry.record_failure(Some(4000), t0);
    }
    assert!(retry.should_attempt(Some(4000), t0 + CEILING_RETRY_INTERVAL));
    assert!(!retry.should_attempt(Some(4000), t0 + Duration::from_secs(1)));
}

/// Disabling the ladder clears the clamp rather than pinning the encoder at
/// rung 0 — the ceiling belongs to the ladder, and an unarmed ladder owns
/// nothing.
#[test]
fn disabling_the_ladder_clears_the_ceiling() {
    assert!(ceiling_needs_apply(Some(Some(1200)), Some(1200), None));
    assert!(!ceiling_needs_apply(Some(None), None, None));
}

/// End-to-end over a real unix socket + a real sidecar file: a rung change
/// reaches `ados-video`'s command socket with the ladder's bitrate, and the
/// steady state that follows sends nothing.
#[tokio::test]
async fn reconcile_publishes_over_the_socket_then_goes_quiet() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("video-encoder.sock");
    let sidecar = dir.path().join("video-profile.json");
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();

    // Stand in for ados-video: accept one request, echo an applied state.
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen_srv = seen.clone();
    let server = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                continue;
            }
            seen_srv.lock().await.push(line.trim().to_string());
            let reply = br#"{"ok":true,"profile":"hero","ceiling_kbps":1200,"width":1280,"height":720,"fps":30,"bitrate_kbps":1200}
"#;
            let _ = reader.into_inner().write_all(reply).await;
        }
    });

    let mut ctrl =
        BitrateController::with_tiers(DEFAULT_TIERS.to_vec(), true, DEFAULT_TICK_INTERVAL)
            .with_encoder_paths(&sock, &sidecar);

    // No sidecar yet and nothing asserted: a 1200 ceiling must be published.
    ctrl.reconcile_encoder_ceiling(Some(1200), None).await;
    assert_eq!(ctrl.asserted_ceiling_kbps, Some(1200));
    {
        let s = seen.lock().await;
        assert_eq!(s.len(), 1, "exactly one publish");
        assert!(
            s[0].contains("video.encoder.ceiling.set") && s[0].contains("1200"),
            "unexpected request: {}",
            s[0]
        );
    }

    // ados-video now publishes 1200 as live: the next reconcile is silent.
    std::fs::write(
        &sidecar,
        br#"{"profile":"hero","ceiling_kbps":1200,"width":1280,"height":720,"fps":30,"bitrate_kbps":1200}"#,
    )
    .unwrap();
    let observed = ados_video::profile::read_state_from(&sidecar).expect("sidecar parses");
    ctrl.reconcile_encoder_ceiling(Some(1200), Some(&observed))
        .await;
    assert_eq!(seen.lock().await.len(), 1, "steady state must send nothing");

    server.abort();
}

/// `ados-video` unreachable: the reconcile must not panic, must not claim the
/// ceiling was asserted, and must therefore retry on the next rung change.
#[tokio::test]
async fn reconcile_survives_an_unreachable_encoder() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("absent.sock");
    let sidecar = dir.path().join("absent.json");
    let mut ctrl =
        BitrateController::with_tiers(DEFAULT_TIERS.to_vec(), true, DEFAULT_TICK_INTERVAL)
            .with_encoder_paths(&sock, &sidecar);

    ctrl.reconcile_encoder_ceiling(Some(1200), None).await;
    assert_eq!(
        ctrl.asserted_ceiling_kbps, None,
        "a failed publish must not be recorded as asserted"
    );
    assert!(ceiling_needs_apply(
        None,
        ctrl.asserted_ceiling_kbps,
        Some(1200)
    ));
}
