use super::*;

#[test]
fn missing_file_yields_python_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let c = GroundStationConfig::load_from(&dir.path().join("nope.yaml"));
    assert_eq!(c.wfb_relay.receiver_mdns_service, "_ados-receiver._tcp");
    assert_eq!(c.wfb_relay.receiver_port, 5800);
    assert_eq!(c.wfb_receiver.listen_port, 5800);
    assert!(c.wfb_receiver.accept_local_nic);
    assert_eq!(c.mesh.bat_iface, "bat0");
}

#[test]
fn reads_ground_station_block() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(
        &cfg,
        "ground_station:\n  wfb_relay:\n    receiver_port: 5900\n  wfb_receiver:\n    listen_port: 5901\n    accept_local_nic: false\n  mesh:\n    bat_iface: bat1\n",
    )
    .unwrap();
    let c = GroundStationConfig::load_from(&cfg);
    assert_eq!(c.wfb_relay.receiver_port, 5900);
    assert_eq!(c.wfb_receiver.listen_port, 5901);
    assert!(!c.wfb_receiver.accept_local_nic);
    assert_eq!(c.mesh.bat_iface, "bat1");
    // Unset relay mDNS service falls back to the default.
    assert_eq!(c.wfb_relay.receiver_mdns_service, "_ados-receiver._tcp");
}

#[test]
fn other_blocks_dont_break_parse() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(
        &cfg,
        "agent:\n  profile: ground_station\nvideo:\n  wfb:\n    channel: 36\n",
    )
    .unwrap();
    let c = GroundStationConfig::load_from(&cfg);
    // No ground_station block → all defaults.
    assert_eq!(c.wfb_receiver.listen_port, 5800);
}

#[test]
fn config_slice_conforms() {
    ados_config::schema::assert_slice_conforms::<GroundStationConfig>(CONFIG_POINTER);
}
