use super::*;

#[test]
fn absent_section_is_enabled_with_defaults() {
    let cfg = read_config_from("agent:\n  name: x\n");
    assert!(cfg.enabled);
    assert_eq!(cfg.expected, "auto");
    assert_eq!(cfg.cooldown, Duration::from_secs(DEFAULT_COOLDOWN_S));
    assert_eq!(cfg.debounce, Duration::from_secs(DEFAULT_DEBOUNCE_S));
    assert!(!cfg.allow_hub_reset);
    assert!(cfg.allow_ppps);
    assert!(!cfg.allow_shared_hub_reset);
}

#[test]
fn explicit_tunables_and_expected() {
    let cfg = read_config_from(
        "video:\n  camera:\n    expected: \"true\"\n  usb_recovery:\n    enabled: false\n    debounce_s: 5\n    cooldown_s: 25\n    allow_hub_reset: true\n    allow_ppps: false\n    allow_shared_hub_reset: true\n",
    );
    assert!(!cfg.enabled);
    assert_eq!(cfg.expected, "true");
    assert_eq!(cfg.debounce, Duration::from_secs(5));
    assert_eq!(cfg.cooldown, Duration::from_secs(25));
    assert!(cfg.allow_hub_reset);
    assert!(!cfg.allow_ppps);
    assert!(cfg.allow_shared_hub_reset);
}

#[test]
fn malformed_config_defaults_enabled() {
    assert!(read_config_from(": : : not yaml").enabled);
}

#[test]
fn expected_resolution_matrix() {
    // auto = expected iff a last-good record exists.
    assert!(!camera_expected("auto", false));
    assert!(camera_expected("auto", true));
    // Explicit wins regardless of last-good. This is the key anti-false-
    // positive guarantee: a camera-less drone with no last-good stays idle.
    assert!(camera_expected("true", false));
    assert!(!camera_expected("false", true));
}
