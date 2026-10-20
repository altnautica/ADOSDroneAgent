use super::discovery::is_rig_lidar_on_visor;
use super::*;
use crate::param_cache::ParamCache;
use crate::state::VehicleState;

#[test]
fn visor_excludes_a_cp2102_port_from_the_sweep() {
    assert!(is_rig_lidar_on_visor("visor", 0x10C4, 0xEA60));
}

#[test]
fn drone_keeps_a_cp2102_port_in_the_sweep() {
    assert!(!is_rig_lidar_on_visor("drone", 0x10C4, 0xEA60));
    assert!(!is_rig_lidar_on_visor("visor", 0x1209, 0x5740));
}

#[test]
fn visor_keeps_an_explicitly_configured_serial_port() {
    let state = std::sync::Arc::new(Mutex::new(VehicleState::default()));
    let params = std::sync::Arc::new(Mutex::new(ParamCache::new(
        "/tmp/ados-lidar-exclusion-params.json",
    )));
    let c = FcConnection::new(
        MavlinkConfig {
            serial_port: "/dev/ttyUSB0".into(),
            profile: "visor".into(),
            ..MavlinkConfig::default()
        },
        state,
        params,
    );
    let cands = c.candidate_ports();
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].name, "/dev/ttyUSB0");
}
