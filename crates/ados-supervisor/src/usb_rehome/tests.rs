use super::*;

#[test]
fn usb_rehome_sidecar_version_matches_registry() {
    // The per-file const and the sidecar registry are the two sources of
    // truth for this sidecar's schema version; a drift is caught here. The
    // reader (`ados-control`'s status route) gates its drift warning on the
    // registry, so an unregistered sidecar would warn on every read.
    assert_eq!(
        USB_REHOME_SIDECAR_VERSION,
        ados_protocol::contracts::sidecar_version("usb-rehome").unwrap()
    );
}

#[test]
fn absent_section_is_enabled_with_defaults() {
    let cfg = read_config_from("agent:\n  name: x\n");
    assert!(cfg.enabled);
    assert_eq!(cfg.cooldown, Duration::from_secs(DEFAULT_COOLDOWN_S));
    assert_eq!(
        cfg.healthy_reset,
        Duration::from_secs(DEFAULT_HEALTHY_RESET_S)
    );
}

#[test]
fn explicit_disable_and_tunables() {
    let cfg = read_config_from(
        "network:\n  usb_rehome:\n    enabled: false\n    cooldown_s: 20\n    healthy_reset_s: 90\n",
    );
    assert!(!cfg.enabled);
    assert_eq!(cfg.cooldown, Duration::from_secs(20));
    assert_eq!(cfg.healthy_reset, Duration::from_secs(90));
}

#[test]
fn a_zero_cooldown_floors_rather_than_hot_looping() {
    let cfg = read_config_from("network:\n  usb_rehome:\n    cooldown_s: 0\n");
    assert_eq!(cfg.cooldown, Duration::from_secs(1));
}

#[test]
fn malformed_config_defaults_enabled() {
    assert!(read_config_from(": : : not yaml").enabled);
}
