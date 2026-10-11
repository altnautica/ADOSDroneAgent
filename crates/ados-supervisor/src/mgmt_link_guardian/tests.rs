use super::*;

#[test]
fn mgmt_link_sidecar_version_matches_registry() {
    // The per-file const and the sidecar registry are the two sources of
    // truth for this sidecar's schema version; a drift is caught here.
    assert_eq!(
        MGMT_LINK_SIDECAR_VERSION,
        ados_protocol::contracts::sidecar_version("mgmt-link").unwrap()
    );
}

#[test]
fn absent_section_is_enabled_with_defaults() {
    let cfg = read_config_from("agent:\n  name: x\n");
    assert!(cfg.enabled);
    assert_eq!(
        cfg.tick_interval,
        Duration::from_secs(DEFAULT_TICK_INTERVAL_S)
    );
    assert_eq!(cfg.fail_threshold, DEFAULT_FAIL_THRESHOLD);
    assert_eq!(cfg.repairs_per_window, DEFAULT_REPAIRS_PER_WINDOW);
    assert_eq!(
        cfg.repair_window,
        Duration::from_secs(DEFAULT_REPAIR_WINDOW_S)
    );
}

#[test]
fn explicit_disable_is_honored() {
    let cfg = read_config_from("network:\n  management_link_guardian:\n    enabled: false\n");
    assert!(!cfg.enabled);
}

#[test]
fn explicit_tunables_parse_and_floor() {
    let body = "network:\n  management_link_guardian:\n    tick_interval_s: 15\n    fail_threshold: 3\n    repairs_per_window: 8\n    repair_window_s: 300\n";
    let cfg = read_config_from(body);
    assert_eq!(cfg.tick_interval, Duration::from_secs(15));
    assert_eq!(cfg.fail_threshold, 3);
    assert_eq!(cfg.repairs_per_window, 8);
    assert_eq!(cfg.repair_window, Duration::from_secs(300));
    // Zeros floor to 1 so the guardian can never spin or treat 0 as a cap.
    let zero = read_config_from(
        "network:\n  management_link_guardian:\n    tick_interval_s: 0\n    fail_threshold: 0\n    repairs_per_window: 0\n    repair_window_s: 0\n",
    );
    assert_eq!(zero.tick_interval, Duration::from_secs(1));
    assert_eq!(zero.fail_threshold, 1);
    assert_eq!(zero.repairs_per_window, 1);
    assert_eq!(zero.repair_window, Duration::from_secs(1));
}

#[test]
fn malformed_config_defaults_enabled() {
    let cfg = read_config_from(": : : not yaml");
    assert!(cfg.enabled);
    assert_eq!(cfg.fail_threshold, DEFAULT_FAIL_THRESHOLD);
}

#[test]
fn fast_window_zero_disables_fast_path() {
    let cfg =
        read_config_from("network:\n  management_link_guardian:\n    fast_initial_window_s: 0\n");
    assert_eq!(cfg.fast_initial_window, Duration::ZERO);
    assert_eq!(cfg.effective_interval(Duration::ZERO), cfg.tick_interval);
}

#[test]
fn effective_interval_fast_then_steady() {
    let cfg = MgmtGuardianConfig::default();
    assert_eq!(
        cfg.effective_interval(Duration::from_secs(0)),
        cfg.fast_initial_tick
    );
    assert_eq!(
        cfg.effective_interval(Duration::from_secs(59)),
        cfg.fast_initial_tick
    );
    assert_eq!(
        cfg.effective_interval(Duration::from_secs(60)),
        cfg.tick_interval
    );
}

fn guardian() -> MgmtLinkGuardian {
    MgmtLinkGuardian::new(EventEmitter::with_socket(
        "ados-test",
        "/nonexistent/ados/logd.sock",
    ))
}

#[tokio::test]
async fn due_when_never_ticked_then_throttled() {
    let g = guardian();
    let now = Instant::now();
    assert!(g.due(Duration::from_secs(30), now));
    let mut g2 = guardian();
    g2.last_tick = Some(now);
    assert!(!g2.due(Duration::from_secs(30), now + Duration::from_secs(10)));
    assert!(g2.due(Duration::from_secs(30), now + Duration::from_secs(31)));
}

#[tokio::test]
async fn repairs_in_window_prunes_old_entries() {
    let mut g = guardian();
    let t0 = Instant::now();
    let window = Duration::from_secs(600);
    g.repair_times.push_back(t0);
    g.repair_times.push_back(t0 + Duration::from_secs(100));
    // Both inside the window.
    assert_eq!(
        g.repairs_in_window(t0 + Duration::from_secs(200), window),
        2
    );
    // At t0+650 the first entry (age 650s) ages out past the 600s window but
    // the second (age 550s) is still inside it.
    assert_eq!(
        g.repairs_in_window(t0 + Duration::from_secs(650), window),
        1
    );
}

#[test]
fn sidecar_round_trips_through_the_atomic_writer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mgmt-link.json");
    let snap = MgmtLinkSidecar {
        version: MGMT_LINK_SIDECAR_VERSION,
        state: "degraded".to_string(),
        iface: "eth0".to_string(),
        transport: "ethernet".to_string(),
        backend: "networkd".to_string(),
        carrier: true,
        has_lease: true,
        gateway_reachable: false,
        repairing: true,
        last_rung: Some("renew_dhcp".to_string()),
        last_repair_at_unix: Some(1_717_000_000),
        repairs_in_window: 2,
        updated_at_unix: 1_717_000_001,
    };
    write_json_atomic(&path, &snap, 0o644).unwrap();
    let reloaded: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(reloaded["state"], "degraded");
    assert_eq!(reloaded["gateway_reachable"], false);
    assert_eq!(reloaded["last_rung"], "renew_dhcp");
    assert_eq!(reloaded["repairs_in_window"], 2);
    // No leftover temp sibling.
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn tick_is_inert_on_dev_host() {
    // On a non-Linux dev host the tick is a no-op; on Linux CI it stays inert
    // (no `ip`, or no managed backend) and must never panic.
    let mut g = guardian();
    g.tick().await;
}
