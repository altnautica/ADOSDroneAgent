use super::*;
use std::io::Write;

fn write(path: &Path, contents: &str) {
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(contents.as_bytes()).unwrap();
}

#[test]
fn missing_file_yields_python_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let c = MavlinkConfig::load_from(&dir.path().join("nope.yaml"));
    assert_eq!(c.serial_port, "");
    assert_eq!(c.baud_rate, 57600);
    assert_eq!(c.system_id, 1);
    assert_eq!(c.component_id, 191);
    assert_eq!(c.websocket_port(), Some(8765));
}

#[test]
fn reads_explicit_mavlink_section_and_ignores_others() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    write(
            &cfg,
            "agent:\n  profile: drone\nmavlink:\n  serial_port: /dev/ttyACM0\n  baud_rate: 921600\n  system_id: 1\n  component_id: 191\nvideo:\n  mode: auto\n",
        );
    let c = MavlinkConfig::load_from(&cfg);
    assert_eq!(c.serial_port, "/dev/ttyACM0");
    assert_eq!(c.baud_rate, 921600);
    // endpoints omitted -> default websocket 8765
    assert_eq!(c.websocket_port(), Some(8765));
    // The foreign `agent:` section is read for the profile gate.
    assert_eq!(c.profile, "drone");
    assert!(c.is_drone());
}

#[test]
fn an_explicit_profile_wins_over_the_marker_file() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("profile.conf");
    write(&marker, "profile: ground_station\n");
    // The config is explicit, so the marker is not consulted.
    assert_eq!(resolve_profile(Some("drone"), &marker), "drone");
    // And the hyphen form is the wire spelling of a ground station.
    assert_eq!(
        resolve_profile(Some("ground_station"), &dir.path().join("absent")),
        "ground-station"
    );
}

#[test]
fn an_auto_or_absent_profile_falls_back_to_the_marker_file() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("profile.conf");
    write(&marker, "# a comment\nprofile: ground_station\n");
    assert_eq!(resolve_profile(Some("auto"), &marker), "ground-station");
    assert_eq!(resolve_profile(None, &marker), "ground-station");
    assert_eq!(resolve_profile(Some(""), &marker), "ground-station");
    // The legacy key=value spelling reads the same.
    let legacy = dir.path().join("legacy.conf");
    write(&legacy, "profile=drone\n");
    assert_eq!(resolve_profile(Some("auto"), &legacy), "drone");
}

#[test]
fn an_unresolvable_profile_reads_as_a_drone() {
    // Matches how the process supervisor resolves the same question when it
    // decides which units to start, so a sloppy config cannot leave this
    // service running with one notion of its profile and gating on another.
    let dir = tempfile::tempdir().unwrap();
    let absent = dir.path().join("absent.conf");
    assert_eq!(resolve_profile(None, &absent), "drone");
    assert_eq!(resolve_profile(Some("auto"), &absent), "drone");
    assert_eq!(resolve_profile(Some("nonsense"), &absent), "drone");
    // A marker with no profile line is no answer either.
    let empty = dir.path().join("empty.conf");
    write(&empty, "# nothing here\n");
    assert_eq!(resolve_profile(Some("auto"), &empty), "drone");
}

#[test]
fn the_other_profiles_are_not_mistaken_for_drones() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("profile.conf");
    for (written, expected) in [
        ("workstation", "workstation"),
        ("compute", "compute"),
        ("visor", "visor"),
    ] {
        write(&marker, &format!("profile: {written}\n"));
        let resolved = resolve_profile(Some("auto"), &marker);
        assert_eq!(resolved, expected);
        let cfg = MavlinkConfig {
            profile: resolved,
            ..Default::default()
        };
        assert!(!cfg.is_drone());
    }
    // An explicit config value names a visor too.
    assert_eq!(
        resolve_profile(Some("visor"), &dir.path().join("absent.conf")),
        "visor"
    );
}

#[test]
fn partial_mavlink_section_fills_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    write(&cfg, "mavlink:\n  serial_port: /dev/ttyAMA0\n");
    let c = MavlinkConfig::load_from(&cfg);
    assert_eq!(c.serial_port, "/dev/ttyAMA0");
    assert_eq!(c.baud_rate, 57600);
    assert_eq!(c.component_id, 191);
}

#[test]
fn explicit_endpoints_override_default() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    write(
            &cfg,
            "mavlink:\n  endpoints:\n    - type: websocket\n      host: 0.0.0.0\n      port: 9000\n      enabled: true\n",
        );
    let c = MavlinkConfig::load_from(&cfg);
    assert_eq!(c.websocket_port(), Some(9000));
}

#[test]
fn disabled_websocket_endpoint_is_not_selected() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    write(
        &cfg,
        "mavlink:\n  endpoints:\n    - type: websocket\n      port: 8765\n      enabled: false\n",
    );
    let c = MavlinkConfig::load_from(&cfg);
    assert_eq!(c.websocket_port(), None);
}

#[test]
fn ws_proxy_enforce_auth_defaults_on() {
    let dir = tempfile::tempdir().unwrap();
    // A missing file and a config that omits the flag both enforce, so a
    // node that was never reconfigured is protected rather than left open
    // waiting for someone to opt in.
    assert!(MavlinkConfig::load_from(&dir.path().join("nope.yaml")).ws_proxy_enforce_auth);
    let cfg = dir.path().join("config.yaml");
    write(&cfg, "mavlink:\n  serial_port: /dev/ttyACM0\n");
    assert!(MavlinkConfig::load_from(&cfg).ws_proxy_enforce_auth);
}

#[test]
fn ws_proxy_enforce_auth_can_still_be_turned_off_explicitly() {
    // An operator with a third-party client that cannot present a
    // credential needs a way out that is deliberate and recorded in their
    // config, rather than being the silent default for everyone.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    write(&cfg, "mavlink:\n  ws_proxy_enforce_auth: false\n");
    assert!(!MavlinkConfig::load_from(&cfg).ws_proxy_enforce_auth);
}

#[test]
fn raw_proxy_enforce_auth_defaults_on() {
    // A missing file and a config that omits the flag must both refuse
    // unauthorized raw peers: the raw edges carry no credential, so an
    // open default hands FC write to the whole LAN of an unpaired node.
    let dir = tempfile::tempdir().unwrap();
    assert!(MavlinkConfig::load_from(&dir.path().join("nope.yaml")).raw_proxy_enforce_auth);
    let cfg = dir.path().join("config.yaml");
    write(&cfg, "mavlink:\n  serial_port: /dev/ttyACM0\n");
    assert!(MavlinkConfig::load_from(&cfg).raw_proxy_enforce_auth);
    // An operator can still turn it off, on record in their config.
    write(&cfg, "mavlink:\n  raw_proxy_enforce_auth: false\n");
    assert!(!MavlinkConfig::load_from(&cfg).raw_proxy_enforce_auth);
}

#[test]
fn a_paired_nodes_raw_edges_stay_closed_to_the_lan_unless_opted_in() {
    // A node whose config never mentions the key keeps its raw proxies
    // on-box once paired; serving them to the LAN is a recorded choice.
    let dir = tempfile::tempdir().unwrap();
    assert!(!MavlinkConfig::load_from(&dir.path().join("nope.yaml")).raw_proxy_lan_access);
    let cfg = dir.path().join("config.yaml");
    write(&cfg, "mavlink:\n  raw_proxy_enforce_auth: false\n");
    assert!(!MavlinkConfig::load_from(&cfg).raw_proxy_lan_access);
    write(&cfg, "mavlink:\n  raw_proxy_lan_access: true\n");
    assert!(MavlinkConfig::load_from(&cfg).raw_proxy_lan_access);
}

#[test]
fn raw_proxy_enforce_auth_can_be_turned_on_explicitly() {
    // The lockdown exists and is reachable: an operator running only
    // first-party clients on a paired node can close the raw edges.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    write(&cfg, "mavlink:\n  raw_proxy_enforce_auth: true\n");
    assert!(MavlinkConfig::load_from(&cfg).raw_proxy_enforce_auth);
}

#[test]
fn aux_uplink_enforce_origin_defaults_off() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!MavlinkConfig::load_from(&dir.path().join("nope.yaml")).aux_uplink_enforce_origin);
    let cfg = dir.path().join("config.yaml");
    write(&cfg, "mavlink:\n  serial_port: /dev/ttyACM0\n");
    assert!(!MavlinkConfig::load_from(&cfg).aux_uplink_enforce_origin);
}

#[test]
fn aux_uplink_enforce_origin_can_be_turned_on_explicitly() {
    // A fleet that has finished credentialing its clients can close the
    // relay edge without waiting for a build.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    write(&cfg, "mavlink:\n  aux_uplink_enforce_origin: true\n");
    assert!(MavlinkConfig::load_from(&cfg).aux_uplink_enforce_origin);
}

#[test]
fn crsf_device_pin_reads_from_the_radio_section() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    write(
        &cfg,
        "mavlink:\n  serial_port: /dev/ttyACM0\nradio:\n  crsf:\n    device: \" /dev/ttyUSB0 \"\n",
    );
    let c = MavlinkConfig::load_from(&cfg);
    // The pin is read from the top-level radio section and trimmed; the
    // mavlink block itself is untouched by it.
    assert_eq!(c.crsf_device, "/dev/ttyUSB0");
    assert_eq!(c.serial_port, "/dev/ttyACM0");
}

#[test]
fn crsf_device_pin_defaults_empty() {
    let dir = tempfile::tempdir().unwrap();
    // Missing file, and a config with no radio block, both read no pin.
    assert_eq!(
        MavlinkConfig::load_from(&dir.path().join("nope.yaml")).crsf_device,
        ""
    );
    let cfg = dir.path().join("config.yaml");
    write(&cfg, "mavlink:\n  serial_port: /dev/ttyACM0\n");
    assert_eq!(MavlinkConfig::load_from(&cfg).crsf_device, "");
    // A crsf block with no device also reads no pin.
    let cfg2 = dir.path().join("config2.yaml");
    write(&cfg2, "radio:\n  crsf:\n    enabled: true\n");
    assert_eq!(MavlinkConfig::load_from(&cfg2).crsf_device, "");
    // The config model writes `device: null` for "no pin": the explicit
    // null reads as no pin and must not fail the whole mavlink parse.
    let cfg3 = dir.path().join("config3.yaml");
    write(
        &cfg3,
        "mavlink:\n  serial_port: /dev/ttyACM0\nradio:\n  crsf:\n    device: null\n",
    );
    let c3 = MavlinkConfig::load_from(&cfg3);
    assert_eq!(c3.crsf_device, "");
    assert_eq!(c3.serial_port, "/dev/ttyACM0");
}

#[test]
fn crsf_mavlink_source_resolves_serial_from_the_radio_section() {
    // The lane opted in, in MAVLink mode, with a pinned device: the
    // router owns the pin at the fixed MAVLink-mode baud.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    write(
        &cfg,
        "radio:\n  crsf:\n    enabled: true\n    device: /dev/ttyUSB0\n    mode: mavlink\n",
    );
    let c = MavlinkConfig::load_from(&cfg);
    assert_eq!(
        c.crsf_mavlink_source(),
        Some(CrsfMavlinkSource::Serial {
            device: "/dev/ttyUSB0"
        })
    );
}

#[test]
fn crsf_mavlink_command_enabled_defaults_off_and_reads_explicit() {
    let dir = tempfile::tempdir().unwrap();
    // A MAVLink-over-ELRS source with no explicit marker is command-down
    // gated: the flag reads false, so the connection runs it telemetry-only.
    let gated = dir.path().join("gated.yaml");
    write(
        &gated,
        "radio:\n  crsf:\n    enabled: true\n    device: /dev/ttyUSB0\n    mode: mavlink\n",
    );
    let c = MavlinkConfig::load_from(&gated);
    assert!(c.crsf_mavlink_source().is_some());
    assert!(!c.crsf_mavlink_command_enabled);
    // The marker is read verbatim when present — the bench-gate switch.
    let armed = dir.path().join("armed.yaml");
    write(
            &armed,
            "radio:\n  crsf:\n    enabled: true\n    device: /dev/ttyUSB0\n    mode: mavlink\n    mavlink_command_enabled: true\n",
        );
    assert!(MavlinkConfig::load_from(&armed).crsf_mavlink_command_enabled);
    // A missing file defaults the marker off.
    assert!(!MavlinkConfig::load_from(&dir.path().join("nope.yaml")).crsf_mavlink_command_enabled);
}

#[test]
fn crsf_mavlink_serial_without_a_pin_does_not_resolve() {
    // The fixed baud is not in the discovery sweep, so an unpinned module
    // is an unresolved source — never a guessed port.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    write(
        &cfg,
        "radio:\n  crsf:\n    enabled: true\n    device: null\n    mode: mavlink\n",
    );
    assert_eq!(MavlinkConfig::load_from(&cfg).crsf_mavlink_source(), None);
}

#[test]
fn crsf_backpack_wifi_resolves_without_a_pin() {
    // The backpack carrier is a fixed local UDP listen; no serial pin is
    // involved, so it resolves on the mode + transport alone.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    write(
            &cfg,
            "radio:\n  crsf:\n    enabled: true\n    mode: mavlink\n    mavlink_transport: backpack_wifi\n",
        );
    assert_eq!(
        MavlinkConfig::load_from(&cfg).crsf_mavlink_source(),
        Some(CrsfMavlinkSource::BackpackUdp { port: 14550 })
    );
}

#[test]
fn crsf_rc_mode_or_opted_out_lane_never_resolves_a_source() {
    let dir = tempfile::tempdir().unwrap();
    // Default mode (crsf_rc): the RC lane owns the port; no ingest source.
    let rc = dir.path().join("rc.yaml");
    write(
        &rc,
        "radio:\n  crsf:\n    enabled: true\n    device: /dev/ttyUSB0\n",
    );
    assert_eq!(MavlinkConfig::load_from(&rc).crsf_mavlink_source(), None);
    // MAVLink mode but the lane opted out: no source either.
    let off = dir.path().join("off.yaml");
    write(
        &off,
        "radio:\n  crsf:\n    enabled: false\n    device: /dev/ttyUSB0\n    mode: mavlink\n",
    );
    assert_eq!(MavlinkConfig::load_from(&off).crsf_mavlink_source(), None);
    // No radio block at all, and a missing file: both resolve nothing.
    let bare = dir.path().join("bare.yaml");
    write(&bare, "mavlink:\n  serial_port: /dev/ttyACM0\n");
    assert_eq!(MavlinkConfig::load_from(&bare).crsf_mavlink_source(), None);
    assert_eq!(
        MavlinkConfig::load_from(&dir.path().join("nope.yaml")).crsf_mavlink_source(),
        None
    );
}

#[test]
fn unknown_mavlink_transport_degrades_to_the_serial_carrier() {
    // The lane service parses an unknown transport as its serial default;
    // the router must interpret the same block the same way.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    write(
            &cfg,
            "radio:\n  crsf:\n    enabled: true\n    device: /dev/ttyUSB0\n    mode: mavlink\n    mavlink_transport: carrier_pigeon\n",
        );
    assert_eq!(
        MavlinkConfig::load_from(&cfg).crsf_mavlink_source(),
        Some(CrsfMavlinkSource::Serial {
            device: "/dev/ttyUSB0"
        })
    );
}

#[test]
fn crsf_mode_flip_re_resolves_on_the_next_config_load() {
    // The router reads its config at startup; the persist path kicks the
    // unit on a router-relevant crsf change. A flip must resolve cleanly
    // in both directions from the same file.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    write(
        &cfg,
        "radio:\n  crsf:\n    enabled: true\n    device: /dev/ttyUSB0\n    mode: mavlink\n",
    );
    assert!(MavlinkConfig::load_from(&cfg)
        .crsf_mavlink_source()
        .is_some());
    write(
        &cfg,
        "radio:\n  crsf:\n    enabled: true\n    device: /dev/ttyUSB0\n    mode: crsf_rc\n",
    );
    let back = MavlinkConfig::load_from(&cfg);
    assert_eq!(back.crsf_mavlink_source(), None);
    // Back in RC mode the pin is once again an exclusion, not a source.
    assert_eq!(back.crsf_device, "/dev/ttyUSB0");
}

#[test]
fn ws_proxy_enforce_auth_reads_explicit_true() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    write(&cfg, "mavlink:\n  ws_proxy_enforce_auth: true\n");
    assert!(MavlinkConfig::load_from(&cfg).ws_proxy_enforce_auth);
}

#[test]
fn source_defaults_to_auto_and_reads_explicit() {
    let dir = tempfile::tempdir().unwrap();
    // Missing file and an omitted field both default to "auto".
    assert_eq!(
        MavlinkConfig::load_from(&dir.path().join("nope.yaml")).source,
        "auto"
    );
    let cfg = dir.path().join("config.yaml");
    write(&cfg, "mavlink:\n  serial_port: /dev/ttyACM0\n");
    assert_eq!(MavlinkConfig::load_from(&cfg).source, "auto");
    // An explicit value is read verbatim.
    let cfg2 = dir.path().join("config2.yaml");
    write(
        &cfg2,
        "mavlink:\n  source: serial\n  serial_port: /dev/ttyACM0\n",
    );
    assert_eq!(MavlinkConfig::load_from(&cfg2).source, "serial");
}

#[test]
fn arbitration_inactive_on_drone() {
    // The arbiter runs only on a ground station; a drone with the flag on
    // must not build a gate that refuses every injector forever.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    write(
        &cfg,
        "agent:\n  profile: drone\nmavlink:\n  injector_arbitration: true\n",
    );
    let c = MavlinkConfig::load_from(&cfg);
    assert!(c.injector_arbitration);
    assert!(!c.injector_arbitration_active());
    assert_eq!(c.injector_arbitration_state(), "inactive_no_local_arbiter");

    write(
        &cfg,
        "agent:\n  profile: ground_station\nmavlink:\n  injector_arbitration: true\n",
    );
    let gs = MavlinkConfig::load_from(&cfg);
    assert!(gs.injector_arbitration_active());
    assert_eq!(gs.injector_arbitration_state(), "active");

    write(&cfg, "agent:\n  profile: ground_station\nmavlink: {}\n");
    assert_eq!(
        MavlinkConfig::load_from(&cfg).injector_arbitration_state(),
        "off"
    );
}

#[test]
fn config_slice_conforms() {
    ados_config::schema::assert_slice_conforms::<MavlinkConfig>("/mavlink")
}
