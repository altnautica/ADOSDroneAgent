use super::*;
use crate::modules::ModuleRole;
use std::io::Write;

fn write_file(dir: &tempfile::TempDir, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.path().join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    path
}

#[test]
fn missing_file_reads_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = CrsfLaneConfig::load_from(&dir.path().join("nope.yaml"));
    assert_eq!(cfg, CrsfLaneConfig::default());
    assert!(!cfg.enabled);
    assert_eq!(cfg.packet_rate_hz, DEFAULT_PACKET_RATE_HZ);
    assert_eq!(cfg.band, BandMode::Dual);
    assert_eq!(cfg.tx_power_dbm, None);
    assert_eq!(cfg.mode, LaneMode::CrsfRc);
    assert_eq!(cfg.channel_source, ChannelSourceMode::Hid);
    assert_eq!(cfg.mavlink_transport, MavlinkTransport::Serial);
    assert_eq!(cfg.relay_role, RelayRole::None);
    assert!(!cfg.mavlink_command_enabled);
    assert!(cfg.modules.is_empty());
    assert!(cfg.resolved_modules().is_empty());
}

/// Parse a `radio.crsf` slice body straight through the section type.
fn from_yaml(body: &str) -> CrsfLaneConfig {
    CrsfLaneConfig::from_section(serde_norway::from_str(body).unwrap())
}

#[test]
fn two_module_config_parses_and_resolves() {
    let cfg = from_yaml(
        "enabled: true\nmodules:\n  - id: m0\n    port: /dev/ttyACM0\n    role: pilot\n  - id: m1\n    port: \" /dev/ttyACM1 \"\n    role: autonomy\n    link: survey-a\n",
    );
    let resolved = cfg.resolved_modules();
    assert_eq!(
        resolved,
        vec![
            ModuleConfig {
                id: "m0".to_string(),
                port: "/dev/ttyACM0".to_string(),
                role: ModuleRole::Pilot,
                link: None,
            },
            ModuleConfig {
                id: "m1".to_string(),
                port: "/dev/ttyACM1".to_string(),
                role: ModuleRole::Autonomy,
                link: Some("survey-a".to_string()),
            },
        ]
    );
    // The configured list wins over a legacy pin.
    let both = from_yaml(
        "device: /dev/ttyUSB0\nmodules:\n  - id: m1\n    port: /dev/ttyACM1\n    role: autonomy\n",
    );
    let resolved = both.resolved_modules();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].id, "m1");
}

#[test]
fn legacy_single_port_config_resolves_to_one_pilot_m0() {
    let cfg = from_yaml("enabled: true\ndevice: /dev/ttyUSB0\n");
    assert!(cfg.modules.is_empty());
    assert_eq!(
        cfg.resolved_modules(),
        vec![ModuleConfig {
            id: "m0".to_string(),
            port: "/dev/ttyUSB0".to_string(),
            role: ModuleRole::Pilot,
            link: None,
        }]
    );
    // No pin and no list: no module at all, never a portless m0.
    assert!(from_yaml("enabled: true\ndevice: null\n")
        .resolved_modules()
        .is_empty());
}

#[test]
fn invalid_module_entries_are_dropped() {
    let cfg = from_yaml(
        "modules:\n  - id: m0\n    port: /dev/ttyACM0\n  - id: m1\n    port: /dev/ttyACM1\n    role: pilot\n  - id: m2\n    port: /dev/ttyACM0\n    role: autonomy\n  - id: m0\n    port: /dev/ttyACM3\n    role: autonomy\n  - port: /dev/ttyACM4\n    role: autonomy\n  - id: m5\n    role: autonomy\n  - id: m6\n    port: /dev/ttyACM6\n    role: copilot\n  - id: m7\n    port: /dev/ttyACM7\n    role: autonomy\n",
    );
    // An absent role reads pilot; a second pilot, a repeated port or id, a
    // missing id or port, and an unknown role are each dropped.
    let ids: Vec<(&str, ModuleRole)> = cfg
        .modules
        .iter()
        .map(|m| (m.id.as_str(), m.role))
        .collect();
    assert_eq!(
        ids,
        vec![("m0", ModuleRole::Pilot), ("m7", ModuleRole::Autonomy)]
    );
}

#[test]
fn full_block_reads_through() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(
        &dir,
        "config.yaml",
        "radio:\n  crsf:\n    enabled: true\n    device: \" /dev/ttyUSB0 \"\n    band: \"900\"\n    packet_rate_hz: 250\n    tx_power_dbm: 20\n    mode: mavlink\n    channel_source: hybrid\n    mavlink_transport: backpack_wifi\n    relay_role: repeater\n    mavlink_command_enabled: true\n",
    );
    let cfg = CrsfLaneConfig::load_from(&path);
    assert!(cfg.enabled);
    assert_eq!(cfg.device, "/dev/ttyUSB0");
    assert_eq!(cfg.band, BandMode::Band900);
    assert_eq!(cfg.packet_rate_hz, 250);
    assert_eq!(cfg.tx_power_dbm, Some(20));
    assert_eq!(cfg.mode, LaneMode::Mavlink);
    assert_eq!(cfg.channel_source, ChannelSourceMode::Hybrid);
    assert_eq!(cfg.mavlink_transport, MavlinkTransport::BackpackWifi);
    assert_eq!(cfg.relay_role, RelayRole::Repeater);
    assert!(cfg.mavlink_command_enabled);
}

#[test]
fn fc_command_down_gated_is_tri_state_and_mirrors_the_router() {
    // No MAVLink-over-ELRS source -> None (nothing to gate). The RC channel
    // lane, an unconfigured lane, and a disabled lane all read absent.
    let rc = CrsfLaneConfig {
        enabled: true,
        device: "/dev/ttyUSB0".to_string(),
        mode: LaneMode::CrsfRc,
        ..Default::default()
    };
    assert_eq!(rc.fc_command_down_gated(), None);
    let disabled = CrsfLaneConfig {
        enabled: false,
        device: "/dev/ttyUSB0".to_string(),
        mode: LaneMode::Mavlink,
        ..Default::default()
    };
    assert_eq!(disabled.fc_command_down_gated(), None);
    // mode: mavlink with a serial carrier but no pinned device does not
    // resolve a source (matching the router) -> None.
    let no_device = CrsfLaneConfig {
        enabled: true,
        device: String::new(),
        mode: LaneMode::Mavlink,
        mavlink_transport: MavlinkTransport::Serial,
        ..Default::default()
    };
    assert_eq!(no_device.fc_command_down_gated(), None);

    // A resolved serial source, gate closed by default -> Some(true).
    let serial_gated = CrsfLaneConfig {
        enabled: true,
        device: "/dev/ttyUSB0".to_string(),
        mode: LaneMode::Mavlink,
        mavlink_transport: MavlinkTransport::Serial,
        mavlink_command_enabled: false,
        ..Default::default()
    };
    assert_eq!(serial_gated.fc_command_down_gated(), Some(true));
    // Same source with the command marker set -> the gate is open, Some(false).
    let serial_open = CrsfLaneConfig {
        mavlink_command_enabled: true,
        ..serial_gated.clone()
    };
    assert_eq!(serial_open.fc_command_down_gated(), Some(false));
    // The backpack carrier always resolves, even with no pinned device.
    let backpack = CrsfLaneConfig {
        enabled: true,
        device: String::new(),
        mode: LaneMode::Mavlink,
        mavlink_transport: MavlinkTransport::BackpackWifi,
        ..Default::default()
    };
    assert_eq!(backpack.fc_command_down_gated(), Some(true));
}

#[test]
fn null_device_and_power_read_as_unset() {
    // The Python model writes explicit nulls for the unset nullable
    // fields; the parse must read them as absent, never fail.
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(
        &dir,
        "config.yaml",
        "radio:\n  crsf:\n    enabled: true\n    device: null\n    tx_power_dbm: null\n",
    );
    let cfg = CrsfLaneConfig::load_from(&path);
    assert!(cfg.enabled);
    assert!(cfg.device.is_empty());
    assert_eq!(cfg.tx_power_dbm, None);
}

#[test]
fn out_of_range_rate_clamps() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(
        &dir,
        "config.yaml",
        "radio:\n  crsf:\n    enabled: true\n    packet_rate_hz: 2000\n",
    );
    assert_eq!(
        CrsfLaneConfig::load_from(&path).packet_rate_hz,
        MAX_PACKET_RATE_HZ
    );
    let path0 = write_file(
        &dir,
        "config0.yaml",
        "radio:\n  crsf:\n    packet_rate_hz: 0\n",
    );
    assert_eq!(CrsfLaneConfig::load_from(&path0).packet_rate_hz, 1);
}

#[test]
fn channel_source_parses_with_a_hid_default() {
    let dir = tempfile::tempdir().unwrap();
    for (yaml, expected) in [
        (
            "radio:\n  crsf:\n    enabled: true\n",
            ChannelSourceMode::Hid,
        ),
        (
            "radio:\n  crsf:\n    channel_source: hid\n",
            ChannelSourceMode::Hid,
        ),
        (
            "radio:\n  crsf:\n    channel_source: inject\n",
            ChannelSourceMode::Inject,
        ),
        (
            "radio:\n  crsf:\n    channel_source: hybrid\n",
            ChannelSourceMode::Hybrid,
        ),
        (
            "radio:\n  crsf:\n    channel_source: bogus\n",
            ChannelSourceMode::Hid,
        ),
    ] {
        let path = write_file(&dir, "config.yaml", yaml);
        assert_eq!(
            CrsfLaneConfig::load_from(&path).channel_source,
            expected,
            "yaml: {yaml:?}"
        );
    }
}

#[test]
fn unknown_enum_values_degrade_to_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(
        &dir,
        "config.yaml",
        "radio:\n  crsf:\n    band: 5ghz\n    mode: teleport\n    mavlink_transport: carrier_pigeon\n    relay_role: chief\n",
    );
    let cfg = CrsfLaneConfig::load_from(&path);
    assert_eq!(cfg.band, BandMode::Dual);
    assert_eq!(cfg.mode, LaneMode::CrsfRc);
    assert_eq!(cfg.mavlink_transport, MavlinkTransport::Serial);
    assert_eq!(cfg.relay_role, RelayRole::None);
}

#[test]
fn enum_round_trips_cover_every_variant() {
    for band in [BandMode::Dual, BandMode::Band900, BandMode::Band2p4] {
        assert_eq!(BandMode::parse(band.as_str()), Some(band));
    }
    for mode in [LaneMode::CrsfRc, LaneMode::Mavlink, LaneMode::Airport] {
        assert_eq!(LaneMode::parse(mode.as_str()), Some(mode));
    }
    for t in [MavlinkTransport::Serial, MavlinkTransport::BackpackWifi] {
        assert_eq!(MavlinkTransport::parse(t.as_str()), Some(t));
    }
    for r in [
        RelayRole::None,
        RelayRole::Repeater,
        RelayRole::AgentLastMile,
    ] {
        assert_eq!(RelayRole::parse(r.as_str()), Some(r));
    }
    // The sidecar projection reports participation only: `none` is null.
    assert_eq!(RelayRole::None.sidecar_str(), None);
    assert_eq!(RelayRole::Repeater.sidecar_str(), Some("repeater"));
    assert_eq!(
        RelayRole::AgentLastMile.sidecar_str(),
        Some("agent_last_mile")
    );
}

#[test]
fn absent_crsf_block_is_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(&dir, "config.yaml", "video:\n  enabled: true\n");
    let cfg = CrsfLaneConfig::load_from(&path);
    assert!(!cfg.enabled);
    assert!(cfg.device.is_empty());
}

#[test]
fn profile_gate_reads_config_then_marker() {
    let dir = tempfile::tempdir().unwrap();
    let drone = write_file(&dir, "drone.yaml", "agent:\n  profile: drone\n");
    let gs = write_file(&dir, "gs.yaml", "agent:\n  profile: ground_station\n");
    let unset = write_file(&dir, "unset.yaml", "agent: {}\n");
    let marker_drone = write_file(&dir, "profile-drone.conf", "profile=drone\n");
    let marker_gs = write_file(&dir, "profile-gs.conf", "profile: ground_station\n");
    let missing = dir.path().join("missing");

    assert!(profile_is_drone(&drone, &missing));
    assert!(!profile_is_drone(&gs, &marker_drone), "config wins");
    assert!(profile_is_drone(&unset, &marker_drone));
    assert!(!profile_is_drone(&unset, &marker_gs));
    assert!(!profile_is_drone(&missing, &missing), "unknown never gates");
}

#[test]
fn config_slice_conforms() {
    ados_config::schema::assert_slice_conforms::<CrsfSection>("/radio/crsf");
}
