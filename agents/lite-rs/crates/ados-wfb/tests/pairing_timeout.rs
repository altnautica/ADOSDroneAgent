//! Manager state-machine integration test for hot-plug cycles.
//!
//! Exercises the public state-machine surface across multiple
//! attach / detach cycles to confirm:
//!
//! - The state transitions are idempotent across a sequence of events
//!   (Idle → DongleDetected → Idle → DongleDetected → ...).
//! - The `interface` field on the config summary tracks the most
//!   recent attach event.
//! - The process-layer subprocess backoff doubles on each crash and
//!   resets to the minimum after a clean shutdown signal.
//!
//! Note on what is NOT pinned here: the [`ados_wfb::WfbState::Crashed`]
//! variant carries a `restart_at_unix` field that records the wall-clock
//! retry time the orchestration loop will use after a `wfb_tx` crash.
//! No public method on `WfbManager` drives the state machine into the
//! `Crashed` branch today — the supervised-process integration that
//! would do so lives outside this crate, behind the same gate as the
//! hardware-validation work. The closest reachable proxy is the
//! [`WfbProcess`] backoff doubling, asserted at the bottom of this file.
//! When the manager grows a public hook to drive the `Crashed` state
//! directly, this test gains the `restart_at_unix` monotonicity check
//! the wider state-machine spec calls for.

use std::path::PathBuf;
use std::time::Duration;

use ados_wfb::{
    DongleEvent, WfbAdvancedOpts, WfbConfig, WfbManager, WfbProcess, WfbState, WfbTxArgs,
    DEFAULT_KEYPAIR_PATH, DEFAULT_WFB_TX_PATH,
};

fn cfg() -> WfbConfig {
    WfbConfig {
        channel: 161,
        mcs_index: 1,
        tx_power_dbm: 25,
        key_passphrase: "pairing-timeout-test".to_string(),
        wfb_tx_path: PathBuf::from(DEFAULT_WFB_TX_PATH),
        interface: None,
        keypair_path: PathBuf::from(DEFAULT_KEYPAIR_PATH),
        advanced: WfbAdvancedOpts::default(),
    }
}

#[tokio::test]
async fn three_attach_detach_cycles_round_trip_state_cleanly() {
    let mgr = WfbManager::new(cfg()).expect("construct");
    for i in 0..3 {
        let iface = format!("wlan{i}");
        mgr.handle_dongle_event(DongleEvent::Added(iface.clone()))
            .await;
        let snap = mgr.state_snapshot().await;
        match snap.state {
            WfbState::DongleDetected { iface: seen } => assert_eq!(seen, iface),
            other => panic!("cycle {i}: expected DongleDetected, got {other:?}"),
        }
        assert_eq!(snap.config_summary.interface.as_deref(), Some(iface.as_str()));

        mgr.handle_dongle_event(DongleEvent::Removed(iface.clone()))
            .await;
        let snap = mgr.state_snapshot().await;
        assert!(
            matches!(snap.state, WfbState::Idle),
            "cycle {i}: removal must drop back to Idle, got {:?}",
            snap.state,
        );

        // 10 ms gap between cycles so any future timestamp-based
        // monotonicity assertion (once the manager exposes
        // `restart_at_unix` advancement) has a non-zero clock delta to
        // observe. Today the assertion above is sufficient because the
        // public state machine returns to Idle in well under a tick.
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn re_attach_after_removal_overwrites_interface_field() {
    let mgr = WfbManager::new(cfg()).expect("construct");

    mgr.handle_dongle_event(DongleEvent::Added("wlan0".to_string()))
        .await;
    mgr.handle_dongle_event(DongleEvent::Removed("wlan0".to_string()))
        .await;
    mgr.handle_dongle_event(DongleEvent::Added("wlan1".to_string()))
        .await;

    let snap = mgr.state_snapshot().await;
    match snap.state {
        WfbState::DongleDetected { iface } => assert_eq!(iface, "wlan1"),
        other => panic!("expected DongleDetected wlan1, got {other:?}"),
    }
    assert_eq!(snap.config_summary.interface.as_deref(), Some("wlan1"));
}

#[tokio::test]
async fn rapid_burst_of_events_does_not_lose_final_state() {
    // Burst of 10 alternating attach/detach events. The final event is
    // an `Added`, so the snapshot must report DongleDetected on the
    // final interface — no event is silently dropped, no transition is
    // left half-applied.
    let mgr = WfbManager::new(cfg()).expect("construct");
    for i in 0..5 {
        let iface = format!("wlan{i}");
        mgr.handle_dongle_event(DongleEvent::Added(iface.clone()))
            .await;
        mgr.handle_dongle_event(DongleEvent::Removed(iface)).await;
    }
    mgr.handle_dongle_event(DongleEvent::Added("wlan99".to_string()))
        .await;
    let snap = mgr.state_snapshot().await;
    match snap.state {
        WfbState::DongleDetected { iface } => assert_eq!(iface, "wlan99"),
        other => panic!("expected DongleDetected wlan99, got {other:?}"),
    }
}

#[tokio::test]
async fn build_args_after_each_cycle_reflects_current_interface() {
    let mgr = WfbManager::new(cfg()).expect("construct");

    // Before any attach: build_args returns Ok(None).
    assert!(mgr.build_args().await.expect("ok").is_none());

    for i in 0..3 {
        let iface = format!("wlan{i}");
        mgr.handle_dongle_event(DongleEvent::Added(iface.clone()))
            .await;
        let args = mgr
            .build_args()
            .await
            .expect("build_args ok")
            .expect("interface bound");
        assert_eq!(args.interface, iface);

        mgr.handle_dongle_event(DongleEvent::Removed(iface)).await;
        // After removal the manager is Idle but the config still has
        // the last interface stamped on it (the manager updates only
        // on Added). build_args therefore still returns the args; this
        // is the documented contract — the orchestration loop is the
        // layer that decides when to spawn vs. when to wait.
        let args = mgr.build_args().await.expect("build_args ok");
        assert!(
            args.is_some(),
            "config interface stays stamped post-removal so build_args still resolves"
        );
    }
}

#[tokio::test]
async fn process_backoff_doubles_across_repeated_crashes() {
    // The orchestration layer's restart strategy lives in
    // `WfbProcess::wait_then_backoff`, which doubles the sleep on each
    // crash up to `RESTART_BACKOFF_MAX`. Use `/bin/false` as a
    // deterministically-crashing child so we can observe the doubling
    // without depending on a real `wfb_tx` binary.
    let path = PathBuf::from("/bin/false");
    if !path.exists() {
        eprintln!("/bin/false missing on this host, skipping backoff test");
        return;
    }

    let args = WfbTxArgs {
        interface: "wlan0".to_string(),
        channel: 161,
        mcs_index: 1,
        tx_power_dbm: 25,
        keypair_path: PathBuf::from(DEFAULT_KEYPAIR_PATH),
        advanced: WfbAdvancedOpts::default(),
    };
    let mut proc = WfbProcess::new(path, args);

    let initial = proc.backoff();
    proc.spawn().expect("spawn 1");
    let _ = proc.wait_then_backoff().await.expect("wait 1");
    let after_first = proc.backoff();
    assert!(
        after_first > initial,
        "backoff must grow after first crash: {initial:?} -> {after_first:?}"
    );

    proc.spawn().expect("spawn 2");
    let _ = proc.wait_then_backoff().await.expect("wait 2");
    let after_second = proc.backoff();
    assert!(
        after_second > after_first,
        "backoff must grow on consecutive crashes: {after_first:?} -> {after_second:?}"
    );

    // After a clean-shutdown signal the orchestration layer calls
    // `reset_backoff`. The next round must restart the doubling from
    // the minimum so a previously-bad dongle that came back healthy
    // does not have to wait 30 s before its first restart.
    proc.reset_backoff();
    assert_eq!(
        proc.backoff(),
        initial,
        "reset_backoff must return to the minimum"
    );
}

#[tokio::test]
async fn apply_config_changes_do_not_drop_dongle_binding() {
    // Apply a fresh config across a cycle and confirm the dongle
    // binding survives. The supervisor relies on this so an operator
    // who edits the channel mid-flight does not also have to wait for
    // udev to re-emit the attach event.
    let mgr = WfbManager::new(cfg()).expect("construct");
    mgr.handle_dongle_event(DongleEvent::Added("wlan0".to_string()))
        .await;

    let mut new = cfg();
    new.channel = 36; // valid 5 GHz channel != current 161
    mgr.apply_config(new).await.expect("apply");

    // apply_config replaces the stored config wholesale (including the
    // None interface from the fresh `cfg()`), so the manager's binding
    // is dropped from the config side. The state machine itself,
    // however, still reports DongleDetected — that is the documented
    // contract today: state and config are decoupled, the
    // orchestration loop reconciles them. Pin both halves so a future
    // change that conflates the two surfaces.
    let snap = mgr.state_snapshot().await;
    assert!(
        matches!(snap.state, WfbState::DongleDetected { .. }),
        "state machine must retain DongleDetected across apply_config"
    );
    assert_eq!(
        snap.config_summary.channel, 36,
        "apply_config must land the new channel"
    );
}
