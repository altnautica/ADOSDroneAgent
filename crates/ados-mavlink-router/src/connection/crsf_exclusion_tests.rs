use super::*;
use crate::param_cache::ParamCache;
use crate::state::VehicleState;

fn conn_with(cfg: MavlinkConfig) -> std::sync::Arc<FcConnection> {
    let state = std::sync::Arc::new(Mutex::new(VehicleState::default()));
    let params = std::sync::Arc::new(Mutex::new(ParamCache::new(
        "/tmp/ados-crsf-exclusion-params.json",
    )));
    FcConnection::new(cfg, state, params)
}

#[test]
fn pinned_fc_port_survives_when_distinct_from_the_crsf_pin() {
    let c = conn_with(MavlinkConfig {
        serial_port: "/dev/ttyACM0".into(),
        crsf_device: "/dev/ttyUSB0".into(),
        crsf_enabled: true,
        ..MavlinkConfig::default()
    });
    let cands = c.candidate_ports();
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].name, "/dev/ttyACM0");
    // An explicitly configured port carries no USB id (no vendor gating).
    assert_eq!(cands[0].usb, None);
}

#[test]
fn crsf_pin_excludes_a_matching_configured_fc_port() {
    // With the lane ENABLED, the pin is the operator's explicit statement
    // that the device is the RC module; a contradictory FC config must not
    // open it.
    let c = conn_with(MavlinkConfig {
        serial_port: "/dev/ttyUSB0".into(),
        crsf_device: "/dev/ttyUSB0".into(),
        crsf_enabled: true,
        ..MavlinkConfig::default()
    });
    assert!(c.candidate_ports().is_empty());
}

#[test]
fn disabled_lane_stale_pin_does_not_exclude_the_fc() {
    // The lane is OFF but a stale `radio.crsf.device` pin still equals the
    // operator's FC port. A disabled lane owns no device, so the pin must
    // not drop the flight controller.
    let c = conn_with(MavlinkConfig {
        serial_port: "/dev/ttyACM0".into(),
        crsf_device: "/dev/ttyACM0".into(),
        crsf_enabled: false,
        ..MavlinkConfig::default()
    });
    let cands = c.candidate_ports();
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].name, "/dev/ttyACM0");
}

#[test]
fn every_crsf_module_port_is_excluded() {
    // Two RC modules on the lane: neither port may become the FC, and the
    // legacy pin stays excluded alongside them.
    let two_modules = |serial_port: &str| MavlinkConfig {
        serial_port: serial_port.into(),
        crsf_device: "/dev/ttyUSB0".into(),
        crsf_module_ports: vec!["/dev/ttyACM1".into(), "/dev/ttyACM2".into()],
        crsf_enabled: true,
        ..MavlinkConfig::default()
    };
    for port in ["/dev/ttyACM1", "/dev/ttyACM2", "/dev/ttyUSB0"] {
        let c = conn_with(two_modules(port));
        assert!(c.candidate_ports().is_empty(), "{port} must be excluded");
    }
    let c = conn_with(two_modules("/dev/ttyACM0"));
    assert_eq!(
        c.crsf_pins(),
        vec!["/dev/ttyUSB0", "/dev/ttyACM1", "/dev/ttyACM2"]
    );
    assert_eq!(c.candidate_ports()[0].name, "/dev/ttyACM0");
    // Module ports alone pin the lane, so the unpinned silent-bridge skip is off.
    let modules_only = conn_with(MavlinkConfig {
        crsf_module_ports: vec!["/dev/ttyACM1".into()],
        crsf_enabled: true,
        crsf_mode: "crsf_rc".into(),
        ..MavlinkConfig::default()
    });
    assert!(!modules_only.crsf_rc_lane_unpinned());
    // A disabled lane owns none of them.
    let disabled = conn_with(MavlinkConfig {
        crsf_enabled: false,
        ..two_modules("/dev/ttyACM1")
    });
    assert!(disabled.crsf_pins().is_empty());
    assert_eq!(disabled.candidate_ports()[0].name, "/dev/ttyACM1");
}

#[test]
fn silent_bridge_candidate_is_opened_when_the_rc_lane_is_disabled() {
    // A budget Betaflight / iNav FC on a CH340 bridge is silent on the
    // passive sweep. With the RC lane OFF there is no RC module to protect,
    // so the fallback open MUST NOT be skipped or the FC never appears.
    let c = conn_with(MavlinkConfig {
        crsf_enabled: false,
        ..MavlinkConfig::default()
    });
    // CH340 bridge id, not the last candidate: must still be openable.
    assert!(!c.skip_silent_bridge_candidate(Some((0x1A86, 0x7523)), false));
}

#[test]
fn silent_bridge_candidate_is_skipped_only_for_an_unpinned_rc_lane() {
    let c = conn_with(MavlinkConfig {
        crsf_enabled: true,
        crsf_mode: "crsf_rc".into(),
        ..MavlinkConfig::default()
    });
    assert!(c.crsf_rc_lane_unpinned());
    // Unpinned crsf_rc lane, CP2102 bridge VID, not the last candidate → skip.
    assert!(c.skip_silent_bridge_candidate(Some((0x10C4, 0xEA60)), false));
    // A non-bridge candidate (e.g. an ACM flight controller) is never skipped.
    assert!(!c.skip_silent_bridge_candidate(None, false));
}

#[test]
fn the_sole_or_last_candidate_is_never_skipped() {
    // A silent MSP FC leaves no sweep evidence, so the final candidate must
    // always be tried even behind an RC-bridge id.
    let c = conn_with(MavlinkConfig {
        crsf_enabled: true,
        crsf_mode: "crsf_rc".into(),
        ..MavlinkConfig::default()
    });
    assert!(!c.skip_silent_bridge_candidate(Some((0x1A86, 0x7523)), true));
}

#[test]
fn an_enabled_lane_with_a_real_pin_still_opens_a_different_bridge_fc() {
    // Lane enabled AND a real pin: `candidate_ports` already excludes the
    // pinned RC module, so a DIFFERENT silent bridge candidate (the flight
    // controller) must not be skipped — a pinned lane is no longer the
    // "unpinned" case the skip guards.
    let c = conn_with(MavlinkConfig {
        crsf_enabled: true,
        crsf_mode: "crsf_rc".into(),
        crsf_device: "/dev/ttyUSB0".into(),
        ..MavlinkConfig::default()
    });
    assert!(!c.crsf_rc_lane_unpinned());
    assert!(!c.skip_silent_bridge_candidate(Some((0x1A86, 0x7523)), false));
}

#[test]
fn a_mavlink_mode_lane_never_skips_a_silent_bridge_candidate() {
    // In `mavlink` mode the module IS the FC carrier the router wants to
    // open, so the RC-protection skip must not fire.
    let c = conn_with(MavlinkConfig {
        crsf_enabled: true,
        crsf_mode: "mavlink".into(),
        ..MavlinkConfig::default()
    });
    assert!(!c.crsf_rc_lane_unpinned());
    assert!(!c.skip_silent_bridge_candidate(Some((0x303A, 0x1001)), false));
}

#[test]
fn no_pin_keeps_the_configured_port() {
    let c = conn_with(MavlinkConfig {
        serial_port: "/dev/ttyACM0".into(),
        ..MavlinkConfig::default()
    });
    assert_eq!(c.cfg.crsf_device, "");
    let cands = c.candidate_ports();
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].name, "/dev/ttyACM0");
}

/// A resolved MAVLink-over-ELRS ingest owns the FC source slot: the
/// snapshot reports the live carrier class (`serial` / `udp`), not the
/// operator's `mavlink.source` pick, while an unresolved lane leaves the
/// slot to the ordinary config path.
#[test]
fn crsf_mavlink_mode_owns_the_fc_source_slot() {
    let serial = conn_with(MavlinkConfig {
        crsf_enabled: true,
        crsf_mode: "mavlink".into(),
        crsf_device: "/dev/ttyUSB0".into(),
        ..MavlinkConfig::default()
    });
    assert_eq!(serial.source(), "serial");

    let backpack = conn_with(MavlinkConfig {
        crsf_enabled: true,
        crsf_mode: "mavlink".into(),
        crsf_mavlink_transport: "backpack_wifi".into(),
        ..MavlinkConfig::default()
    });
    assert_eq!(backpack.source(), "udp");

    // RC mode (the default): the pin is an exclusion, never a source —
    // the slot falls through to the configured/auto vocabulary.
    let rc = conn_with(MavlinkConfig {
        crsf_enabled: true,
        crsf_device: "/dev/ttyUSB0".into(),
        ..MavlinkConfig::default()
    });
    assert_eq!(rc.cfg.crsf_mavlink_source(), None);
    assert_eq!(rc.source(), "auto");

    // MAVLink mode on the serial carrier with no pin: unresolved — the
    // slot is not claimed and discovery vocabulary stands.
    let unpinned = conn_with(MavlinkConfig {
        crsf_enabled: true,
        crsf_mode: "mavlink".into(),
        ..MavlinkConfig::default()
    });
    assert_eq!(unpinned.cfg.crsf_mavlink_source(), None);
    assert_eq!(unpinned.source(), "auto");
}
