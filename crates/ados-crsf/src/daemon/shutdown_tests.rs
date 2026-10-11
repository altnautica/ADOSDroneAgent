use super::*;

/// A latched shutdown watch that is already `true` resolves
/// `wait_for_shutdown_flag` immediately — the latch never loses the edge.
#[tokio::test]
async fn shutdown_flag_already_set_resolves_immediately() {
    let (tx, rx) = watch::channel(false);
    tx.send(true).unwrap();
    let mut rx = rx;
    wait_for_shutdown_flag(&mut rx).await;
}

/// With `biased;` and the shutdown arm first, a latched shutdown wins the
/// poll even when a competing arm is also ready in the same instant.
#[tokio::test]
async fn shutdown_arm_wins_over_a_ready_competitor() {
    let (tx, rx) = watch::channel(false);
    tx.send(true).unwrap();
    let mut rx = rx;
    let competitor = std::future::ready(());

    #[derive(Debug, PartialEq)]
    enum Won {
        Shutdown,
        Competitor,
    }
    let won = tokio::select! {
        biased;
        _ = wait_for_shutdown_flag(&mut rx) => Won::Shutdown,
        _ = competitor => Won::Competitor,
    };
    assert_eq!(won, Won::Shutdown);
}

/// A dropped sender (no more shutdown signal possible) also resolves the
/// wait, so a vanished signaller never strands the run loop forever.
#[tokio::test]
async fn shutdown_flag_resolves_on_sender_drop() {
    let (tx, rx) = watch::channel(false);
    let mut rx = rx;
    drop(tx);
    wait_for_shutdown_flag(&mut rx).await;
}

/// Serialize the tests that mutate the process-global `ADOS_RUN_DIR` (env
/// vars are per-process, tests run on parallel threads). The lock is the
/// crate-wide one: these tests share the lib test binary with the sync
/// `paths` and `sidecar` tests that set the same variable.
async fn env_guard() -> tokio::sync::MutexGuard<'static, ()> {
    crate::paths::test_env_guard_async().await
}

/// Poll the sidecar until its `state` reads `want` (bounded), returning
/// the last body seen for the caller's follow-up assertions.
async fn wait_for_state(path: &std::path::Path, want: &str) -> serde_json::Value {
    for _ in 0..400 {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(body) = serde_json::from_str::<serde_json::Value>(&text) {
                if body["state"] == want {
                    return body;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("sidecar at {path:?} never reached state {want:?}");
}

/// The disabled gate writes the honest sidecar and idles until shutdown —
/// the whole-service acid test for "absent/false enabled".
#[tokio::test]
async fn disabled_lane_writes_the_sidecar_and_exits_on_shutdown() {
    let _g = env_guard().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("ADOS_RUN_DIR", dir.path());
    let cfg = CrsfLaneConfig {
        enabled: false,
        ..Default::default()
    };
    let (tx, rx) = watch::channel(false);
    let reload = Arc::new(Notify::new());
    let missing = dir.path().join("missing.yaml");
    let missing_conf = dir.path().join("missing.conf");
    let service =
        tokio::spawn(async move { run_service(&cfg, &missing, &missing_conf, rx, reload).await });
    let path = dir.path().join("crsf-stats.json");
    let body = wait_for_state(&path, "disabled").await;
    assert!(body["rf_unverified"].is_null());
    // The service idles until the shutdown latch flips, then exits.
    tx.send(true).unwrap();
    let exit = tokio::time::timeout(Duration::from_secs(5), service)
        .await
        .expect("clean exit on shutdown")
        .unwrap();
    assert_eq!(exit, RunExit::Shutdown);
    std::env::remove_var("ADOS_RUN_DIR");
}

/// A config reload re-runs the gates in place: a lane enabled through the
/// config surface goes from `disabled` to `unconfigured` (enabled, no
/// device pinned) on one reload notify, with no unit restart.
#[tokio::test]
async fn config_reload_reruns_the_gates_without_a_restart() {
    let _g = env_guard().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("ADOS_RUN_DIR", dir.path());
    let config_path = dir.path().join("config.yaml");
    let profile_conf = dir.path().join("profile.conf");
    std::fs::write(&config_path, "radio:\n  crsf:\n    enabled: false\n").unwrap();

    let (tx, rx) = watch::channel(false);
    let reload = Arc::new(Notify::new());
    let loop_reload = reload.clone();
    let loop_config = config_path.clone();
    let loop_profile = profile_conf.clone();
    let service = tokio::spawn(async move {
        run_until_shutdown(&loop_config, &loop_profile, rx, loop_reload).await
    });

    let sidecar = dir.path().join("crsf-stats.json");
    wait_for_state(&sidecar, "disabled").await;

    // Enable the lane (no device pin) and signal the reload: the fresh
    // pass passes the opt-in gate and parks at the device guard.
    std::fs::write(&config_path, "radio:\n  crsf:\n    enabled: true\n").unwrap();
    reload.notify_one();
    wait_for_state(&sidecar, "unconfigured").await;

    tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), service)
        .await
        .expect("clean exit on shutdown")
        .unwrap();
    std::env::remove_var("ADOS_RUN_DIR");
}

/// In `mavlink` mode the lane holds off the serial device — the MAVLink
/// router owns the carrier — and stands by honestly: state `ready` with
/// the mode reported, no RC transmitted, nothing flyable. The pinned
/// device is a plain file: had the pass ever reached the device guard it
/// would have tried (and failed) to open it and written `unconfigured`,
/// so a stable `ready` proves the mode gate held the lane off the port.
#[tokio::test]
async fn mavlink_mode_holds_off_the_device_and_stands_by_ready() {
    let _g = env_guard().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("ADOS_RUN_DIR", dir.path());
    let device = dir.path().join("ttyFAKE0");
    std::fs::write(&device, b"").unwrap();
    let cfg = CrsfLaneConfig {
        enabled: true,
        device: device.to_string_lossy().into_owned(),
        mode: LaneMode::Mavlink,
        ..Default::default()
    };
    let (tx, rx) = watch::channel(false);
    let reload = Arc::new(Notify::new());
    let missing = dir.path().join("missing.yaml");
    let missing_conf = dir.path().join("missing.conf");
    let service =
        tokio::spawn(async move { run_service(&cfg, &missing, &missing_conf, rx, reload).await });
    let path = dir.path().join("crsf-stats.json");
    let body = wait_for_state(&path, "ready").await;
    assert_eq!(body["mode"], "mavlink");
    assert_eq!(body["flyable"], false, "the RC lane transmits nothing");
    // A resolved MAVLink-over-ELRS source with the command marker off (the
    // default): the host->FC command-down direction is gated closed, and the
    // lane reports it so a consumer of only this lane sees the gate.
    assert_eq!(
        body["fc_command_down_gated"], true,
        "telemetry-only mavlink source reports its command gate closed"
    );
    assert!(
        body["rf_unverified"].is_null(),
        "standing by carries no liveness verdict"
    );
    assert!(
        body["tx_frames_per_s"].is_null() && body["tx_frames_total"].is_null(),
        "no transmit counter may be fabricated while held off"
    );
    tx.send(true).unwrap();
    let exit = tokio::time::timeout(Duration::from_secs(5), service)
        .await
        .expect("clean exit on shutdown")
        .unwrap();
    assert_eq!(exit, RunExit::Shutdown);
    std::env::remove_var("ADOS_RUN_DIR");
}

/// A mode flip is a clean in-place reload, both directions: `crsf_rc`
/// (parked at the device guard — the pinned node is absent) → `mavlink`
/// (standing by `ready`, the port released for the router) → back to
/// `crsf_rc` (the lane reclaims the device guard), with no unit restart.
#[tokio::test]
async fn mode_flips_between_rc_and_mavlink_are_clean_reloads() {
    let _g = env_guard().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("ADOS_RUN_DIR", dir.path());
    let config_path = dir.path().join("config.yaml");
    let profile_conf = dir.path().join("profile.conf");
    let device = dir.path().join("absent-tty");
    let rc_yaml = format!(
        "radio:\n  crsf:\n    enabled: true\n    device: {}\n    mode: crsf_rc\n",
        device.display()
    );
    let mavlink_yaml = format!(
        "radio:\n  crsf:\n    enabled: true\n    device: {}\n    mode: mavlink\n",
        device.display()
    );
    std::fs::write(&config_path, &rc_yaml).unwrap();

    let (tx, rx) = watch::channel(false);
    let reload = Arc::new(Notify::new());
    let loop_reload = reload.clone();
    let loop_config = config_path.clone();
    let loop_profile = profile_conf.clone();
    let service = tokio::spawn(async move {
        run_until_shutdown(&loop_config, &loop_profile, rx, loop_reload).await
    });

    let sidecar = dir.path().join("crsf-stats.json");
    // RC mode with an absent device: the lane owns the port and parks at
    // the open guard.
    wait_for_state(&sidecar, "unconfigured").await;

    // Flip to MAVLink mode: the fresh pass hands the port to the router
    // and stands by.
    std::fs::write(&config_path, &mavlink_yaml).unwrap();
    reload.notify_one();
    let body = wait_for_state(&sidecar, "ready").await;
    assert_eq!(body["mode"], "mavlink");

    // Flip back: the lane reclaims the device guard in place.
    std::fs::write(&config_path, &rc_yaml).unwrap();
    reload.notify_one();
    let body = wait_for_state(&sidecar, "unconfigured").await;
    assert_eq!(body["mode"].as_str(), None);

    tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), service)
        .await
        .expect("clean exit on shutdown")
        .unwrap();
    std::env::remove_var("ADOS_RUN_DIR");
}
