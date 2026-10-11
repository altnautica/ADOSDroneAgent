use super::*;

#[test]
fn absent_section_is_enabled() {
    let cfg = read_config_from("agent:\n  name: x\n");
    assert!(cfg.enabled);
    assert!(!cfg.apply_live_allowed);
    assert!(cfg.overrides.is_empty());
}

#[test]
fn explicit_disable_is_honored() {
    let cfg = read_config_from("network:\n  mac_pin:\n    enabled: false\n");
    assert!(!cfg.enabled);
}

#[test]
fn overrides_and_apply_live_parse() {
    let body = "network:\n  mac_pin:\n    enabled: true\n    apply_live_allowed: true\n    overrides:\n      \"1234:5678\": \"02:11:22:33:44:55\"\n";
    let cfg = read_config_from(body);
    assert!(cfg.enabled);
    assert!(cfg.apply_live_allowed);
    assert_eq!(
        cfg.overrides.get("1234:5678").map(String::as_str),
        Some("02:11:22:33:44:55")
    );
}

#[test]
fn malformed_config_defaults_enabled() {
    let cfg = read_config_from(": : : not yaml");
    assert!(cfg.enabled);
}
