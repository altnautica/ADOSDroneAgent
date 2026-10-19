use super::*;

#[test]
fn parse_u16_handles_hex_and_decimal() {
    assert_eq!(parse_u16("0x3c"), Some(0x3c));
    assert_eq!(parse_u16("0X3D"), Some(0x3d));
    assert_eq!(parse_u16(" 60 "), Some(60));
    assert_eq!(parse_u16("nonsense"), None);
}

#[test]
fn configured_brightness_reads_the_ui_oled_block() {
    let yaml = "ground_station:\n  ui:\n    oled:\n      brightness: 90\n";
    assert_eq!(configured_brightness(Some(yaml)), 90);
    assert_eq!(configured_brightness(Some("agent:\n  name: gs\n")), 204);
    assert_eq!(configured_brightness(Some(": not yaml")), 204);
    assert_eq!(configured_brightness(None), 204);
}

#[test]
fn the_config_watch_reads_the_panel_role() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(&cfg, "agent:\n  profile: visor\n").unwrap();
    let mut watch = ConfigWatch::new(cfg);
    watch.refresh();
    assert_eq!(watch.role, OledRole::Visor);
    assert!(watch.screens.iter().any(|s| s == "cameras"));
}

#[test]
fn probe_addresses_auto_scan_both_when_unpinned() {
    // Unpinned -> auto-probe the two common OLED addresses in order.
    assert_eq!(resolve_probe_addresses(None), vec![0x3C, 0x3D]);
    // Pinned -> honour exactly that address.
    assert_eq!(resolve_probe_addresses(Some("0x3d")), vec![0x3D]);
    assert_eq!(resolve_probe_addresses(Some("60")), vec![60]);
    // A garbage pin falls back to the default single address (not a scan).
    assert_eq!(
        resolve_probe_addresses(Some("xyz")),
        vec![oled::DEFAULT_I2C_ADDR]
    );
}
