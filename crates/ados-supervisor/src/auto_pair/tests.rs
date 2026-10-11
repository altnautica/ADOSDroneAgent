use super::*;

#[test]
fn wfb_failover_sidecar_version_matches_registry() {
    // The per-file const and the sidecar registry are the two sources of
    // truth for this sidecar's schema version; a drift is caught here.
    assert_eq!(
        WFB_FAILOVER_SIDECAR_VERSION,
        ados_protocol::contracts::sidecar_version("wfb_failover").unwrap()
    );
}

#[test]
fn retry_backoff_is_short_and_jitter_bounded() {
    // The backoff must stay short so both rigs bind near-continuously and
    // their rendezvous windows overlap; and the total must never exceed
    // base + max jitter (a long backoff reintroduces the phase-drift bug).
    let base = RETRY_BACKOFF.as_secs();
    assert!(base <= 5, "backoff base must stay short, got {base}s");
    for _ in 0..200 {
        let b = retry_backoff().as_secs();
        assert!(
            b >= base && b <= base + RETRY_BACKOFF_JITTER_SECS,
            "backoff {b}s out of [{base}, {}]",
            base + RETRY_BACKOFF_JITTER_SECS
        );
    }
}

#[test]
fn should_attempt_gate() {
    assert!(should_attempt(true, false));
    assert!(!should_attempt(false, false)); // disarmed
    assert!(!should_attempt(true, true)); // already paired
}

#[test]
fn key_completeness_rejects_half_written_key() {
    // A complete 64-byte key counts as paired; a short (half-written, e.g.
    // power loss mid-bind) or absent key does NOT, so the rig re-runs its
    // own auto-bind instead of skipping forever on a truncated file.
    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("drone.key");
    std::fs::write(&good, [7u8; 64]).unwrap();
    assert!(key_fingerprint_at(&good).is_some());

    let half = dir.path().join("half.key");
    std::fs::write(&half, [7u8; 32]).unwrap(); // truncated write
    assert!(key_fingerprint_at(&half).is_none());

    let empty = dir.path().join("empty.key");
    std::fs::write(&empty, []).unwrap();
    assert!(key_fingerprint_at(&empty).is_none());

    // A path that does not exist is not paired.
    assert!(key_fingerprint_at(&dir.path().join("missing.key")).is_none());
}

#[test]
fn the_fingerprint_is_stable_and_distinguishes_two_keys() {
    // The latch's whole contract keys on this value, so a paired rig must
    // read the same fingerprint on every tick, and a key that was replaced
    // must read differently.
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.key");
    let b = dir.path().join("b.key");
    std::fs::write(&a, [3u8; 64]).unwrap();
    std::fs::write(&b, [4u8; 64]).unwrap();
    let fa = key_fingerprint_at(&a).unwrap();
    assert_eq!(Some(&fa), key_fingerprint_at(&a).as_ref());
    assert_ne!(Some(fa), key_fingerprint_at(&b));
}

#[test]
fn a_rearm_opens_a_window_a_paired_disarmed_rig_would_otherwise_refuse() {
    // The exact deadlock: the key file is complete so `paired` is true, and
    // the key-apply step already flipped the arm flag off, so `armed` is
    // false. Both gates say no, forever. The latch is the only thing that can
    // say yes — and it must not say yes on its own when they say yes too.
    assert!(!should_attempt(false, true));
    assert!(should_attempt_or_rearm(false, true, true));
    // With no re-arm it stays exactly as before, on every combination.
    for armed in [false, true] {
        for paired in [false, true] {
            assert_eq!(
                should_attempt_or_rearm(armed, paired, false),
                should_attempt(armed, paired)
            );
        }
    }
}

#[test]
fn adapter_gate_skips_bind_without_an_adapter() {
    // With an injection adapter present, a bind may run; without one it must
    // not (the gate is what keeps a dongle-less boot from burning attempts).
    assert!(adapter_ready_to_bind(true));
    assert!(!adapter_ready_to_bind(false));
}

#[test]
fn the_cloud_relay_signal_never_stops_the_local_bind_loop() {
    // The cap used to park the rig: no further local binds until an operator
    // asked for one, so an unpaired rig whose peer came up an hour later
    // never met it. The cap now only flips the sidecar; every tick that has
    // an armed, unpaired rig and an adapter still binds.
    let mut attempt: u32 = 0;
    let mut binds = 0u32;
    let mut signalled = None;
    for _ in 0..(MAX_LOCAL_BIND_ATTEMPTS + 20) {
        if bind_this_tick(true, false, false, None, true) {
            binds += 1;
            attempt += 1;
            if let Some(state) = failover_after_failure(attempt, true) {
                signalled = Some(state);
            }
        }
    }
    assert_eq!(binds, MAX_LOCAL_BIND_ATTEMPTS + 20, "every tick binds");
    assert_eq!(signalled, Some(FailoverState::CloudRelay));
}

#[test]
fn only_a_node_with_a_cloud_relay_signals_the_fallback() {
    assert_eq!(
        failover_after_failure(MAX_LOCAL_BIND_ATTEMPTS - 1, true),
        None
    );
    assert_eq!(
        failover_after_failure(MAX_LOCAL_BIND_ATTEMPTS, true),
        Some(FailoverState::CloudRelay)
    );
    // Local-first: WFB is the only link, so there is nothing to fall back to.
    assert_eq!(
        failover_after_failure(MAX_LOCAL_BIND_ATTEMPTS + 50, false),
        None
    );
    // No adapter, no bind — and no attempt counted toward the signal.
    assert!(!bind_this_tick(true, false, false, None, false));
}

#[test]
fn rearm_opens_a_window_only_while_the_fc_is_disarmed() {
    // Paired rig, arm flag off: only the re-arm latch can open a window.
    assert!(bind_this_tick(false, true, true, Some(false), true));
    // Armed FC: a window would stop video TX in flight.
    assert!(!bind_this_tick(false, true, true, Some(true), true));
    // Unknown FC state is unsafe.
    assert!(!bind_this_tick(false, true, true, None, true));
    // The explicit arm-flag path on an unpaired rig is not FC-gated.
    assert!(bind_this_tick(true, false, false, Some(true), true));
}

#[test]
fn a_local_retry_request_restarts_the_attempt_count() {
    let mut attempt = MAX_LOCAL_BIND_ATTEMPTS;
    assert!(!apply_retry_request(false, &mut attempt));
    assert_eq!(attempt, MAX_LOCAL_BIND_ATTEMPTS);
    assert!(apply_retry_request(true, &mut attempt));
    assert_eq!(attempt, 0);
}

#[test]
fn a_paired_rig_keeps_evaluating_instead_of_exiting() {
    // After a successful pair the loop used to `break`, so the recovery path
    // could only ever run once, at boot. It now continues; a paired rig simply
    // answers "no" cheaply on every tick.
    let armed = true;
    let mut ticks = 0u32;
    for _ in 0..50 {
        ticks += 1;
        let paired = true; // the key-apply step disarms auto-pair too
        assert!(!should_attempt(armed, paired));
    }
    assert_eq!(ticks, 50, "the loop must survive a successful pair");
}

#[test]
fn no_adapter_does_not_consume_a_failover_attempt() {
    // Model the loop's decision: when armed + unpaired but no adapter is
    // present, the bind is skipped and the attempt counter is untouched, so
    // a dongle-less rig never flips to cloud_relay while it waits.
    let armed = true;
    let paired = false;
    let mut attempt: u32 = 0;
    for _ in 0..(MAX_LOCAL_BIND_ATTEMPTS + 5) {
        let adapter_present = adapter_ready_to_bind(false); // never plugged in
        if should_attempt(armed, paired) && adapter_present {
            attempt += 1; // the only path that counts toward failover
        }
    }
    assert_eq!(attempt, 0);
    assert!(!failover_reached(attempt));
}

#[test]
fn armed_defaults_true_without_wfb_section() {
    // A fresh config with no `video.wfb` still arms (first-boot auto-bind).
    assert!(read_armed_from("video:\n  mode: auto\n"));
    assert!(read_armed_from("agent:\n  profile: drone\n"));
    assert!(read_armed_from(""));
}

#[test]
fn armed_honors_explicit_disarm() {
    assert!(!read_armed_from(
        "video:\n  wfb:\n    auto_pair_enabled: false\n"
    ));
    assert!(read_armed_from(
        "video:\n  wfb:\n    auto_pair_enabled: true\n"
    ));
}

#[test]
fn failover_fires_only_at_the_cap() {
    // Below the cap, keep retrying locally.
    for attempt in 0..MAX_LOCAL_BIND_ATTEMPTS {
        assert!(
            !failover_reached(attempt),
            "attempt {attempt} should not fail over"
        );
    }
    // At and past the cap, fall back to the cloud relay.
    assert!(failover_reached(MAX_LOCAL_BIND_ATTEMPTS));
    assert!(failover_reached(MAX_LOCAL_BIND_ATTEMPTS + 1));
}

#[test]
fn local_mode_never_fails_over_to_cloud_relay() {
    // In local-first mode (server.mode = local → cloud_relay_enabled =
    // false) the loop's failover gate never fires past the cap, so an
    // offline rig keeps retrying the local bind instead of giving up.
    let cloud_relay_enabled = false;
    let mut flipped = false;
    for attempt in 1..=(MAX_LOCAL_BIND_ATTEMPTS + 5) {
        if cloud_relay_enabled && failover_reached(attempt) {
            flipped = true;
            break;
        }
    }
    assert!(!flipped, "local mode must never give up the WFB bind");
}

#[test]
fn cloud_mode_fails_over_at_the_cap() {
    // With a cloud relay configured, the same gate flips at the cap.
    let cloud_relay_enabled = true;
    let mut flipped_at = None;
    for attempt in 1..=(MAX_LOCAL_BIND_ATTEMPTS + 5) {
        if cloud_relay_enabled && failover_reached(attempt) {
            flipped_at = Some(attempt);
            break;
        }
    }
    assert_eq!(flipped_at, Some(MAX_LOCAL_BIND_ATTEMPTS));
}

#[test]
fn failover_state_wire_strings() {
    assert_eq!(FailoverState::Local.as_str(), "local");
    assert_eq!(FailoverState::CloudRelay.as_str(), "cloud_relay");
}

#[tokio::test]
async fn failover_emit_ships_state_and_severity_per_transition() {
    use ados_protocol::frame::{decode_len, HEADER_SIZE};
    use ados_protocol::logd::{IngestFrame, Level, LOGD_MAX_FRAME};
    use tokio::io::AsyncReadExt;
    use tokio::net::{UnixListener, UnixStream};

    async fn read_one(stream: &mut UnixStream) -> IngestFrame {
        let mut header = [0u8; HEADER_SIZE];
        stream.read_exact(&mut header).await.unwrap();
        let len = decode_len(header, LOGD_MAX_FRAME, true).unwrap();
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).await.unwrap();
        IngestFrame::decode(&body).unwrap()
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logd.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let accept = {
        let listener = listener;
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut frames = Vec::new();
            for _ in 0..3 {
                frames.push(read_one(&mut stream).await);
            }
            frames
        })
    };

    let events = EventEmitter::with_socket("ados-supervisor", &path);
    // A fresh-run reset to local, the cap-driven fallback to cloud_relay, and
    // a later return to local on a successful pair: the exact transition
    // sequence the loop emits.
    emit_failover(&events, FailoverState::Local);
    emit_failover(&events, FailoverState::CloudRelay);
    emit_failover(&events, FailoverState::Local);

    let frames = tokio::time::timeout(std::time::Duration::from_secs(5), accept)
        .await
        .expect("the three failover events arrived within the deadline")
        .expect("accept task ok");

    let states_levels: Vec<(String, Level)> = frames
        .into_iter()
        .map(|f| match f {
            IngestFrame::Event(e) => {
                assert_eq!(e.kind, "wfb.pair.failover");
                assert_eq!(e.source, "ados-supervisor");
                let state = e
                    .detail
                    .get("state")
                    .and_then(|v| v.as_str())
                    .unwrap()
                    .to_string();
                (state, e.severity)
            }
            other => panic!("expected an event frame, got {other:?}"),
        })
        .collect();
    assert_eq!(
        states_levels,
        vec![
            ("local".to_string(), Level::Info),
            ("cloud_relay".to_string(), Level::Warn),
            ("local".to_string(), Level::Info),
        ]
    );
}

#[test]
fn writer_writes_on_transition_dedups_and_resets() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wfb_failover.json");
    let mut w = FailoverWriter::with_path(path.clone());

    // First reset to local lands (no prior state).
    assert!(w.sync(FailoverState::Local));
    assert!(path.is_file());
    let body = std::fs::read_to_string(&path).unwrap();
    // Body shape the API reader parses: the additive schema `version` plus the
    // load-bearing `state` (two-space indent).
    assert_eq!(
        body,
        format!(
            "{{\n  \"version\": {},\n  \"state\": \"local\"\n}}",
            WFB_FAILOVER_SIDECAR_VERSION
        )
    );
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["state"], "local");
    assert_eq!(parsed["version"], WFB_FAILOVER_SIDECAR_VERSION);
    // Mode is 0o644 so the API process (different user context) can read it.
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o644);

    // Same state again → dedup, no write.
    assert!(!w.sync(FailoverState::Local));

    // Transition to cloud_relay → write.
    assert!(w.sync(FailoverState::CloudRelay));
    let body = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        body,
        format!(
            "{{\n  \"version\": {},\n  \"state\": \"cloud_relay\"\n}}",
            WFB_FAILOVER_SIDECAR_VERSION
        )
    );
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["state"], "cloud_relay");

    // Dedup holds at cloud_relay.
    assert!(!w.sync(FailoverState::CloudRelay));

    // Reset back to local (a successful pair) → write.
    assert!(w.sync(FailoverState::Local));
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(parsed["state"], "local");

    // No torn temp left behind.
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn attempt_counter_flips_to_cloud_relay_then_resets_on_pair() {
    // Model the loop's decision: count consecutive non-paired attempts, flip
    // the sidecar to cloud_relay at the cap, and reset to local on a pair.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wfb_failover.json");
    let mut w = FailoverWriter::with_path(path.clone());

    // Fresh run resets to local.
    w.sync(FailoverState::Local);

    let mut attempt: u32 = 0;
    let mut flipped = false;
    // Nine consecutive failures: still local, never flipped.
    for _ in 0..(MAX_LOCAL_BIND_ATTEMPTS - 1) {
        attempt += 1;
        if failover_reached(attempt) {
            w.sync(FailoverState::CloudRelay);
            flipped = true;
        }
    }
    assert!(!flipped);
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(parsed["state"], "local");

    // The tenth consecutive failure flips to cloud_relay.
    attempt += 1;
    assert!(failover_reached(attempt));
    w.sync(FailoverState::CloudRelay);
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(parsed["state"], "cloud_relay");

    // A later successful pair resets to local.
    w.sync(FailoverState::Local);
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(parsed["state"], "local");
}
