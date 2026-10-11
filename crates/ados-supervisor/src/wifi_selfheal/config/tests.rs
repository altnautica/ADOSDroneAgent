use super::*;

#[test]
fn absent_section_is_enabled_with_defaults() {
    let cfg = read_config_from("agent:\n  name: x\n");
    assert!(cfg.enabled);
    assert_eq!(cfg.fail_threshold, DEFAULT_FAIL_THRESHOLD);
    assert_eq!(cfg.cooldown, Duration::from_secs(DEFAULT_COOLDOWN_S));
}

#[test]
fn explicit_disable_is_honored() {
    let cfg = read_config_from("network:\n  wifi_selfheal:\n    enabled: false\n");
    assert!(!cfg.enabled);
}

#[test]
fn explicit_tunables_parse() {
    let body =
        "network:\n  wifi_selfheal:\n    enabled: true\n    fail_threshold: 3\n    cooldown_s: 90\n";
    let cfg = read_config_from(body);
    assert!(cfg.enabled);
    assert_eq!(cfg.fail_threshold, 3);
    assert_eq!(cfg.cooldown, Duration::from_secs(90));
}

#[test]
fn zero_threshold_is_floored_to_one() {
    let cfg = read_config_from("network:\n  wifi_selfheal:\n    fail_threshold: 0\n");
    assert_eq!(cfg.fail_threshold, 1);
}

#[test]
fn malformed_config_defaults_enabled() {
    let cfg = read_config_from(": : : not yaml");
    assert!(cfg.enabled);
    assert_eq!(cfg.fail_threshold, DEFAULT_FAIL_THRESHOLD);
}
