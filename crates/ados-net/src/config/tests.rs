use super::*;

#[test]
fn missing_file_reads_share_uplink_off() {
    let cfg = UplinkConfig::load_from(Path::new("/nonexistent/ados/config.yaml"));
    assert!(!cfg.share_uplink());
}

#[test]
fn reads_share_uplink_true_ignoring_other_sections() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    std::fs::write(
        &path,
        "\
mavlink:
  port: /dev/ttyACM0
ground_station:
  hotspot_ssid: ADOS-GS-1234
  share_uplink: true
video:
  mode: disabled
",
    )
    .unwrap();
    let cfg = UplinkConfig::load_from(&path);
    assert!(cfg.share_uplink());
}

#[test]
fn absent_ground_station_section_reads_off() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    std::fs::write(&path, "mavlink:\n  port: /dev/ttyACM0\n").unwrap();
    let cfg = UplinkConfig::load_from(&path);
    assert!(!cfg.share_uplink());
}

#[test]
fn unparseable_file_reads_off() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    std::fs::write(&path, "this: is: not: valid: yaml: [[[").unwrap();
    let cfg = UplinkConfig::load_from(&path);
    assert!(!cfg.share_uplink());
}

#[test]
fn config_slice_conforms() {
    ados_config::schema::assert_slice_conforms::<GroundStationSection>("/ground_station");
}
