//! Coverage handoff for the WFB-route same-origin gate.
//!
//! The same-origin middleware that protects `/api/v1/setup/wfb*` lives
//! in the `ados-setup` crate (in `origin.rs`); the WFB REST handlers
//! also live in `ados-setup` (in `wfb_handlers.rs`). The `ados-wfb`
//! crate only exposes the [`ados_wfb::WfbManager`] state machine that
//! both layers consume. A direct integration test that drives the gated
//! WFB router from inside `ados-wfb` would require a dev-dependency on
//! `ados-setup`, which already depends on `ados-wfb` — pulling that in
//! would create a cycle Cargo refuses to resolve.
//!
//! The integration test for the gate is therefore mounted at
//! `crates/ados-setup/tests/wfb_origin_gate.rs` (no `ados-setup/src/`
//! file is touched). This file contains the local invariants that the
//! gate test relies on but cannot reach from the other side: the
//! [`WfbManager`] type must be `Send + Sync` so axum can wrap it in an
//! `Arc<Mutex<...>>` extension layer, and the public configuration knobs
//! the configure handler reads must round-trip through `apply_config`
//! without losing the `interface` binding the udev layer stamps onto
//! them.

use std::path::PathBuf;
use std::sync::Arc;

use ados_wfb::{
    DongleEvent, WfbAdvancedOpts, WfbConfig, WfbManager, DEFAULT_KEYPAIR_PATH, DEFAULT_WFB_TX_PATH,
};
use tokio::sync::Mutex;

fn cfg() -> WfbConfig {
    WfbConfig {
        channel: 161,
        mcs_index: 1,
        tx_power_dbm: 25,
        key_passphrase: "origin-gate-local-test".to_string(),
        wfb_tx_path: PathBuf::from(DEFAULT_WFB_TX_PATH),
        interface: None,
        keypair_path: PathBuf::from(DEFAULT_KEYPAIR_PATH),
        advanced: WfbAdvancedOpts::default(),
    }
}

#[test]
fn manager_is_send_and_sync() {
    // The axum `Extension(Arc<Mutex<WfbManager>>)` layer requires
    // `Send + Sync`. If the manager grows a non-Send field in the
    // future this static assertion fails before the gate test even
    // starts, so the diagnostic surfaces on the right crate.
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<WfbManager>();
    assert_send_sync::<Arc<Mutex<WfbManager>>>();
}

#[tokio::test]
async fn arc_mutex_manager_state_snapshot_locks_briefly() {
    // The handler pattern is `mgr.lock().await.state_snapshot().await`.
    // The inner `state_snapshot` itself takes a fresh lock on the
    // internal state, so a long-held outer lock would deadlock. Drive
    // the same call shape twice in a row to confirm the lock is
    // released cleanly between calls.
    let mgr = Arc::new(Mutex::new(WfbManager::new(cfg()).expect("ctor")));

    let snap1 = mgr.lock().await.state_snapshot().await;
    let snap2 = mgr.lock().await.state_snapshot().await;
    assert_eq!(snap1.config_summary.channel, snap2.config_summary.channel);
}

#[tokio::test]
async fn apply_config_preserves_interface_binding_from_dongle_event() {
    // The configure handler builds a fresh `WfbConfig` carrying the
    // operator-supplied tunables AND the `interface` from the live
    // snapshot, then calls `apply_config`. This test pins the round
    // trip the handler depends on: a dongle event sets the interface,
    // a fresh config built from the snapshot keeps it, and
    // `apply_config` does not silently drop it.
    let mgr = WfbManager::new(cfg()).expect("ctor");
    mgr.handle_dongle_event(DongleEvent::Added("wlan0".to_string()))
        .await;
    let snap = mgr.state_snapshot().await;
    let preserved_iface = snap.config_summary.interface.clone();
    assert_eq!(preserved_iface.as_deref(), Some("wlan0"));

    let mut new_cfg = cfg();
    new_cfg.interface = preserved_iface;
    new_cfg.channel = 36; // valid 5 GHz
    mgr.apply_config(new_cfg).await.expect("apply");

    let snap = mgr.state_snapshot().await;
    assert_eq!(snap.config_summary.channel, 36);
    assert_eq!(snap.config_summary.interface.as_deref(), Some("wlan0"));
}

#[tokio::test]
async fn apply_config_rejects_payloads_a_hostile_origin_might_send() {
    // The handler validates the request envelope BEFORE calling
    // `apply_config`. This test pins the manager-side defenses so a
    // request that bypassed the handler validation (e.g., via a future
    // non-HTTP entry point) still gets rejected with a typed error.
    let mgr = WfbManager::new(cfg()).expect("ctor");

    let mut bad = cfg();
    bad.channel = 200; // outside both bands
    assert!(mgr.apply_config(bad).await.is_err());

    let mut bad = cfg();
    bad.mcs_index = 9; // single-stream ceiling is 7
    assert!(mgr.apply_config(bad).await.is_err());

    let mut bad = cfg();
    bad.tx_power_dbm = 99; // 30 dBm safety envelope
    assert!(mgr.apply_config(bad).await.is_err());
}
