use super::*;

#[test]
fn mgmt_failover_sidecar_version_matches_registry() {
    // The per-file const and the sidecar registry are the two sources of
    // truth for this sidecar's schema version; a drift is caught here. The
    // reader (`ados-control`'s status route) gates its drift warning on the
    // registry, so an unregistered sidecar would warn on every read.
    assert_eq!(
        MGMT_FAILOVER_SIDECAR_VERSION,
        ados_protocol::contracts::sidecar_version("mgmt-failover").unwrap()
    );
}

fn machine() -> MgmtFailover {
    MgmtFailover::new(EventEmitter::with_socket(
        "ados-test",
        "/nonexistent/ados/logd.sock",
    ))
}

fn present(ids: &[&str]) -> std::collections::HashSet<String> {
    ids.iter().map(|s| s.to_string()).collect()
}

#[test]
fn unplugged_wired_from_boot_is_never_a_primary() {
    // A built-in eth0 unplugged from boot (no carrier ever) must not be read
    // as a management primary, or a WiFi-managed rig would flap into the
    // heartbeat reach-back forever.
    let mut ever = std::collections::HashSet::new();
    for _ in 0..5 {
        let (has_primary, primary_up) = resolve_primary(
            &[("eth0".to_string(), false)],
            &present(&["eth0"]),
            &mut ever,
        );
        assert!(!has_primary);
        assert!(!primary_up);
    }
    assert!(ever.is_empty());
}

#[test]
fn wired_that_was_up_stays_a_primary_when_it_drops() {
    // A real cable: up on boot, then unplugged. It stays a primary (sticky)
    // so the drop still triggers failover.
    let mut ever = std::collections::HashSet::new();
    let all = present(&["eth0"]);
    let (has_primary, primary_up) = resolve_primary(&[("eth0".to_string(), true)], &all, &mut ever);
    assert!(has_primary && primary_up);
    let (has_primary, primary_up) =
        resolve_primary(&[("eth0".to_string(), false)], &all, &mut ever);
    assert!(has_primary && !primary_up);
}

#[test]
fn a_usb_ethernet_primary_that_vanishes_is_a_down_primary() {
    // The dongle was the management primary; pulling it removes the netdev
    // altogether, so it is in no candidate list this tick. That is a down
    // primary to fail over from, not a box with no wired primary.
    let dongle = "/sys/devices/platform/usb/1-1/1-1:1.0";
    let mut ever = std::collections::HashSet::new();
    let (has_primary, primary_up) = resolve_primary(
        &[(dongle.to_string(), true)],
        &present(&[dongle]),
        &mut ever,
    );
    assert!(has_primary && primary_up);
    let (has_primary, primary_up) = resolve_primary(&[], &present(&["wlan0"]), &mut ever);
    assert!(has_primary, "a vanished primary must still count");
    assert!(!primary_up);
}

#[test]
fn misclassified_injection_iface_does_not_strand_a_dead_primary() {
    // Even if the injection radio is briefly misclassified as a wired iface
    // (with carrier), once it is correctly classified out of the wired set
    // the only remaining wired iface (eth0, never up) is not a primary — so
    // there is no false "down primary". The radio is still present on the
    // box, so it is not a vanished primary either.
    let mut ever = std::collections::HashSet::new();
    let all = present(&["eth0", "wlan1"]);
    let (has_primary, primary_up) = resolve_primary(
        &[("eth0".to_string(), false), ("wlan1".to_string(), true)],
        &all,
        &mut ever,
    );
    assert!(has_primary && primary_up);
    let (has_primary, _) = resolve_primary(&[("eth0".to_string(), false)], &all, &mut ever);
    assert!(!has_primary);
}

#[test]
fn absent_section_is_enabled_with_defaults() {
    let cfg = read_config_from("agent:\n  name: x\n");
    assert!(cfg.enabled);
    assert_eq!(
        cfg.down_debounce,
        Duration::from_secs(DEFAULT_DOWN_DEBOUNCE_S)
    );
    assert_eq!(
        cfg.recover_debounce,
        Duration::from_secs(DEFAULT_RECOVER_DEBOUNCE_S)
    );
}

#[test]
fn explicit_disable_and_tunables() {
    let cfg = read_config_from(
        "network:\n  mgmt_failover:\n    enabled: false\n    down_debounce_s: 30\n    recover_debounce_s: 10\n",
    );
    assert!(!cfg.enabled);
    assert_eq!(cfg.down_debounce, Duration::from_secs(30));
    assert_eq!(cfg.recover_debounce, Duration::from_secs(10));
}

#[test]
fn malformed_config_defaults_enabled() {
    assert!(read_config_from(": : : not yaml").enabled);
}

#[tokio::test]
async fn down_debounce_before_failover() {
    let mut m = machine();
    let t0 = Instant::now();
    let down = Duration::from_secs(20);
    let rec = Duration::from_secs(15);
    // Primary just went down: still debouncing, no failover yet.
    assert_eq!(m.step(true, true, down, rec, t0), FailoverTransition::None);
    assert_eq!(m.mode(), MgmtLinkMode::Primary);
    // Still inside the window.
    assert_eq!(
        m.step(true, true, down, rec, t0 + Duration::from_secs(10)),
        FailoverTransition::None
    );
    // Sustained past the window → fail over to the WiFi heartbeat.
    assert_eq!(
        m.step(true, true, down, rec, t0 + Duration::from_secs(21)),
        FailoverTransition::Entered
    );
    assert_eq!(m.mode(), MgmtLinkMode::WifiHeartbeat);
}

#[tokio::test]
async fn no_wifi_yields_no_reachback() {
    let mut m = machine();
    let t0 = Instant::now();
    let down = Duration::from_secs(20);
    let rec = Duration::from_secs(15);
    m.step(true, false, down, rec, t0);
    assert_eq!(
        m.step(true, false, down, rec, t0 + Duration::from_secs(21)),
        FailoverTransition::Entered
    );
    assert_eq!(m.mode(), MgmtLinkMode::NoReachback);
}

#[tokio::test]
async fn recover_debounce_before_restore_no_flap() {
    let mut m = machine();
    let t0 = Instant::now();
    let down = Duration::from_secs(20);
    let rec = Duration::from_secs(15);
    // Fail over first.
    m.step(true, true, down, rec, t0);
    m.step(true, true, down, rec, t0 + Duration::from_secs(21));
    assert_eq!(m.mode(), MgmtLinkMode::WifiHeartbeat);
    // Primary comes back: must hold healthy for the recover window before
    // restoring (no flap on a brief carrier blip).
    let t1 = t0 + Duration::from_secs(30);
    assert_eq!(m.step(false, true, down, rec, t1), FailoverTransition::None);
    assert_eq!(m.mode(), MgmtLinkMode::WifiHeartbeat);
    // A re-drop inside the recover window resets the healthy timer.
    assert_eq!(
        m.step(true, true, down, rec, t1 + Duration::from_secs(5)),
        FailoverTransition::None
    );
    // Sustained healthy past the recover window → restore.
    let t2 = t1 + Duration::from_secs(10);
    m.step(false, true, down, rec, t2);
    assert_eq!(
        m.step(false, true, down, rec, t2 + Duration::from_secs(16)),
        FailoverTransition::Exited
    );
    assert_eq!(m.mode(), MgmtLinkMode::Primary);
}

#[test]
fn mode_strings_are_bland() {
    assert_eq!(MgmtLinkMode::Primary.as_str(), "primary");
    assert_eq!(MgmtLinkMode::WifiHeartbeat.as_str(), "wifi_heartbeat");
    assert_eq!(MgmtLinkMode::NoReachback.as_str(), "none");
}

#[test]
fn failover_detail_is_bland() {
    let d = failover_detail(
        "entered",
        MgmtLinkMode::WifiHeartbeat,
        Some("wlan0"),
        "primary_carrier_down",
    );
    assert_eq!(d.get("state").and_then(|v| v.as_str()), Some("entered"));
    assert_eq!(
        d.get("mode").and_then(|v| v.as_str()),
        Some("wifi_heartbeat")
    );
    assert_eq!(d.get("iface").and_then(|v| v.as_str()), Some("wlan0"));
}
