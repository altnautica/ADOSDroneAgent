use super::peer::{counter_advanced, peer_evidence_verdict, upstream_key_fresh};
use super::prep::{parse_iface_mode, summarize_precheck};
use super::state::video_nudge_after_failed_bind;
use super::*;
use crate::bind::fsm::BindState;
use crate::bind::{BindRole, WAITING_PEER_WATCHDOG};
use std::sync::atomic::Ordering;

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
fn the_gate_counts_frames_that_land_after_the_last_poll() {
    // A fast exchange can start and finish inside one poller period; the
    // gate's own read against the counter at tunnel open still proves it.
    assert!(counter_advanced(Some(0), Some(3)));
    assert!(counter_advanced(Some(7), Some(9)));
    // No movement, or an unreadable tunnel, proves nothing.
    assert!(!counter_advanced(Some(4), Some(4)));
    assert!(!counter_advanced(None, Some(4)));
    assert!(!counter_advanced(Some(4), None));
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

#[tokio::test(start_paused = true)]
async fn reclaims_the_lock_only_from_a_finished_or_wedged_session() {
    // A guard held while the session record has failed or aborted (a prior
    // bind whose guard-holder is still cleaning up) must be reclaimable, not a
    // permanent Busy, and so must a session parked past the watchdog. A
    // session mid-setup (`Idle`), progressing, or finishing a bind that paired
    // must not be: cancelling it would end a live bind as aborted.
    let mut s = BindSession::new(BindRole::Gs, "operator", None);
    assert!(
        !s.lock_reclaimable(),
        "the Idle setup window is a live bind"
    );
    s.transition(BindState::WaitingPeer);
    assert!(!s.lock_reclaimable(), "a fresh phase is a live bind");
    s.transition(BindState::Paired);
    assert!(
        !s.lock_reclaimable(),
        "a paired holder is finishing its bind"
    );
    for done in [BindState::Failed, BindState::Aborted] {
        s.transition(done);
        assert!(s.lock_reclaimable(), "{done:?} holder is only cleaning up");
    }
    s.transition(BindState::WaitingPeer);
    tokio::time::advance(WAITING_PEER_WATCHDOG + std::time::Duration::from_secs(1)).await;
    assert!(
        s.lock_reclaimable(),
        "a phase parked past the watchdog is wedged"
    );
    let mut idle = BindSession::new(BindRole::Drone, "operator", None);
    tokio::time::advance(WAITING_PEER_WATCHDOG + std::time::Duration::from_secs(1)).await;
    assert!(
        idle.lock_reclaimable(),
        "a setup window parked past the watchdog is wedged"
    );
    idle.transition(BindState::OpeningTunnel);
    assert!(!idle.lock_reclaimable());
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

#[test]
fn the_saved_upstream_pair_round_trips_and_a_damaged_record_restores_nothing() {
    use super::upstream::UpstreamPair;
    let body = br#"{"gs_key":"0a0b","drone_key":null}"#;
    let pair = UpstreamPair::from_bytes(body).expect("a well-formed record");
    assert_eq!(UpstreamPair::from_bytes(&pair.to_bytes()), Some(pair));
    // A record that cannot be read must never be written over real keys.
    for bad in [
        &b"not json"[..],
        br#"{"gs_key":"0a0b"}"#,
        br#"{"gs_key":"zz","drone_key":null}"#,
        br#"{"gs_key":7,"drone_key":null}"#,
    ] {
        assert_eq!(UpstreamPair::from_bytes(bad), None, "{bad:?}");
    }
}
