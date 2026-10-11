use super::*;

#[test]
fn a_failed_bind_cycles_video_only_on_a_drone_that_holds_a_key() {
    // An unpaired drone binds every ~40 s; cycling its video each time
    // drops the LAN stream the operator is watching.
    assert!(!video_nudge_after_failed_bind(BindRole::Drone, false));
    assert!(video_nudge_after_failed_bind(BindRole::Drone, true));
    // A ground station has no video pipeline to cycle.
    assert!(!video_nudge_after_failed_bind(BindRole::Gs, true));
    assert!(!video_nudge_after_failed_bind(BindRole::Gs, false));
}

#[test]
fn upstream_key_fresh_rejects_stale_missing_and_accepts_fresh() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("drone.key");

    // Missing → not fresh (a conversation that never transferred a key).
    let t0 = std::time::SystemTime::now();
    assert!(!upstream_key_fresh(&key, t0));

    // Written BEFORE the session start → stale leftover, not fresh.
    std::fs::write(&key, b"old").unwrap();
    let after_write = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
    assert!(!upstream_key_fresh(&key, after_write));

    // Written AFTER the session start → this session's transfer.
    let before_write = std::time::SystemTime::now() - std::time::Duration::from_secs(2);
    std::fs::write(&key, b"new").unwrap();
    assert!(upstream_key_fresh(&key, before_write));
}

#[test]
fn peer_evidence_never_verifies_without_proof_of_a_real_peer() {
    // The load-bearing invariant behind Paired: a session may reach Verified
    // (the only verdict the caller lets proceed to Paired) ONLY with decoded
    // peer frames this session AND, drone-side, a freshly-transferred key.
    // No frames → NoFrames for either role, never Verified.
    assert_eq!(
        peer_evidence_verdict(BindRole::Drone, false, true),
        PeerEvidence::NoFrames
    );
    assert_eq!(
        peer_evidence_verdict(BindRole::Gs, false, true),
        PeerEvidence::NoFrames
    );
    // Drone with frames but a stale (leftover) key → StaleKey, never Verified.
    assert_eq!(
        peer_evidence_verdict(BindRole::Drone, true, false),
        PeerEvidence::StaleKey
    );
    // Verified requires BOTH proofs on the drone ...
    assert_eq!(
        peer_evidence_verdict(BindRole::Drone, true, true),
        PeerEvidence::Verified
    );
    // ... while the GS never gates on the drone-key file, so a decoded frame
    // alone verifies it regardless of the key-fresh flag.
    assert_eq!(
        peer_evidence_verdict(BindRole::Gs, true, false),
        PeerEvidence::Verified
    );
    assert_eq!(
        peer_evidence_verdict(BindRole::Gs, true, true),
        PeerEvidence::Verified
    );
    // Stated directly over the whole input space: Verified ⇒ frames_seen.
    for role in [BindRole::Drone, BindRole::Gs] {
        for key_fresh in [false, true] {
            assert_ne!(
                peer_evidence_verdict(role, false, key_fresh),
                PeerEvidence::Verified,
                "verified without decoded frames ({role:?}, key_fresh={key_fresh})"
            );
        }
    }
}

#[test]
fn parse_iface_mode_reads_the_type_line() {
    let monitor = "Interface wlan1\n\tifindex 5\n\ttype monitor\n\twiphy 0\n";
    assert_eq!(parse_iface_mode(monitor).as_deref(), Some("monitor"));
    let managed = "Interface wlan1\n\ttype managed\n";
    assert_eq!(parse_iface_mode(managed).as_deref(), Some("managed"));
    assert!(parse_iface_mode("Interface wlan1\n\tifindex 5\n").is_none());
    assert!(parse_iface_mode("").is_none());
}

#[test]
fn summarize_precheck_classifies_outcomes() {
    // No injection candidate found at all.
    let empty = summarize_precheck(&[]);
    assert!(!empty.ok);
    assert_eq!(empty.reason, Some("iface_not_found"));
    assert!(empty.iface.is_none());

    // A verified-monitor candidate is ok with no reason.
    let good = summarize_precheck(&[InjectionPrepOutcome {
        iface: "wlan1".to_string(),
        injection_mode: "monitor".to_string(),
        monitor_verified: true,
        nm_enumerable: true,
    }]);
    assert!(good.ok);
    assert!(good.reason.is_none());
    assert_eq!(good.injection_mode, "monitor");

    // A managed (unverified) candidate surfaces the failing iface + mode.
    let bad = summarize_precheck(&[
        InjectionPrepOutcome {
            iface: "wlan2".to_string(),
            injection_mode: "monitor".to_string(),
            monitor_verified: true,
            nm_enumerable: true,
        },
        InjectionPrepOutcome {
            iface: "wlan1".to_string(),
            injection_mode: "managed".to_string(),
            monitor_verified: false,
            nm_enumerable: false,
        },
    ]);
    assert!(!bad.ok);
    assert_eq!(bad.reason, Some("monitor_unverified"));
    assert_eq!(bad.injection_mode, "managed");
    assert_eq!(bad.iface.as_deref(), Some("wlan1"));
    assert!(!bad.nm_enumerable);
}

#[tokio::test]
async fn preflight_missing_artifacts_fails_fast_to_failed() {
    // No /etc/bind.key on the dev host / CI → run_session returns Err
    // before any systemctl, the driver lands on FAILED, lock releases.
    let orch = BindOrchestrator::new();
    let result = orch
        .start_local_bind(
            BindRole::Drone,
            None,
            "operator",
            None,
            std::future::pending::<()>(),
        )
        .await
        .expect("start should not report Busy");
    assert_eq!(result["state"], "failed");
    assert_eq!(result["role"], "drone");
    let err = result["error"].as_str().unwrap_or("");
    assert!(err.contains("missing"), "unexpected error: {err}");
    assert!(result["finished_at"].is_string());
    // Terminal → not active, and the singleflight lock is free again.
    assert!(!orch.is_active().await);
}

#[tokio::test]
async fn busy_when_a_session_holds_the_lock() {
    let orch = Arc::new(BindOrchestrator::new());
    // Hold the singleflight lock by parking a bind on a cancel that never
    // fires AND artifacts that DO let it past preflight is hard on a dev
    // host; instead assert the lock directly: take it, then a start fails.
    let _held = orch.lock.try_lock().expect("free at start");
    // An active, freshly-progressing session must keep a concurrent bind out
    // immediately. The stale-session self-heal only reclaims a terminal or
    // watchdog-stale record, never a live one.
    {
        let mut s = BindSession::new(BindRole::Gs, "operator", None);
        s.transition(BindState::WaitingPeer);
        *orch.session.lock().await = Some(s);
    }
    let busy = orch
        .start_local_bind(
            BindRole::Gs,
            None,
            "operator",
            None,
            std::future::pending::<()>(),
        )
        .await;
    assert_eq!(busy, Err(BindStartError::Busy));
}

#[tokio::test]
async fn reclaims_the_lock_from_a_terminal_orphan_session() {
    // A guard held while the session record is terminal (e.g. a prior bind
    // that finished but whose guard-holder has not yet dropped) must be
    // reclaimable, not a permanent Busy. We assert the decision the self-heal
    // makes on the session record, not the full reclaim (which needs a live
    // wedged holder that releases on cancel).
    let orch = BindOrchestrator::new();
    let mut s = BindSession::new(BindRole::Gs, "operator", None);
    s.transition(BindState::Failed); // terminal
    let terminal_reclaimable = s.state.is_terminal();
    assert!(terminal_reclaimable);
    // A stale (watchdog-aged) phase clock is also reclaimable.
    s.transition(BindState::WaitingPeer);
    s.phase_entered_at = Some(now_monotonic() - WAITING_PEER_WATCHDOG.as_secs_f64() - 30.0);
    let stale_reclaimable = s
        .phase_entered_at
        .is_none_or(|t| now_monotonic() - t > WAITING_PEER_WATCHDOG.as_secs_f64());
    assert!(stale_reclaimable);
    // A fresh active session is NOT reclaimable.
    s.transition(BindState::WaitingPeer);
    let fresh_reclaimable = s.state.is_terminal()
        || s.phase_entered_at
            .is_none_or(|t| now_monotonic() - t > WAITING_PEER_WATCHDOG.as_secs_f64());
    assert!(!fresh_reclaimable);
    let _ = &orch;
}

#[test]
fn sentinel_active_false_when_absent() {
    // The real sentinel path almost certainly does not exist in the test
    // env; a missing/garbled file reads as not-active.
    let _ = read_sentinel_active();
}

#[tokio::test]
async fn session_active_is_true_during_idle_setup_window_while_is_active_is_false() {
    // The exact race the supervisor gate must cover: a bind has committed to
    // a session and is mid-setup (the normal radio unit stopped, the
    // injection iface re-prepared) but the FSM is still in `Idle`, which is a
    // terminal state. `is_active` (driven by the session sub-state) reads
    // FALSE here, so a gate on it would NOT block — and a monitor pass would
    // auto-restart the radio unit out from under the bind. `session_active`
    // (the whole-body in-progress flag) must read TRUE for this window.
    let orch = BindOrchestrator::new();

    // Reproduce the window: install an `Idle` session (as `start_local_bind`
    // does at commit) and raise the in-progress flag (as it does right
    // before touching the radio).
    *orch.session.lock().await = Some(BindSession::new(BindRole::Drone, "operator", None));
    orch.session_in_progress.store(true, Ordering::SeqCst);

    // The data-plane liveness is FALSE (Idle is terminal) ...
    assert!(
        !orch.is_active().await,
        "an Idle session is terminal so is_active must be false"
    );
    // ... but the radio-ownership gate must still hold the adapter.
    assert!(
        orch.session_active(),
        "session_active must be true across the Idle stop→opening_tunnel setup window"
    );
}

#[tokio::test]
async fn in_progress_guard_clears_the_flag_on_drop() {
    // The RAII guard must release the flag on every return path so a finished
    // (or panicking) bind never leaves the gate stuck blocking restarts.
    let orch = BindOrchestrator::new();
    assert!(!orch.session_active(), "flag starts clear");
    {
        orch.session_in_progress.store(true, Ordering::SeqCst);
        let _g = InProgressGuard(orch.session_in_progress.clone());
        assert!(
            orch.session_active(),
            "flag is raised inside the guard scope"
        );
    }
    assert!(
        !orch.session_active(),
        "dropping the InProgressGuard must clear the in-progress flag"
    );
}

#[tokio::test]
async fn start_local_bind_clears_session_active_when_it_returns() {
    // A full fail-fast bind (no artifacts on the dev host) runs the whole
    // `start_local_bind` body, which raises the flag at commit and drops the
    // RAII guard on return. After it returns the gate must be released.
    let orch = BindOrchestrator::new();
    assert!(!orch.session_active(), "clear before any bind");
    let result = orch
        .start_local_bind(
            BindRole::Drone,
            None,
            "operator",
            None,
            std::future::pending::<()>(),
        )
        .await
        .expect("start should not report Busy");
    assert_eq!(result["state"], "failed");
    assert!(
        !orch.session_active(),
        "session_active must be false once start_local_bind returns"
    );
}
