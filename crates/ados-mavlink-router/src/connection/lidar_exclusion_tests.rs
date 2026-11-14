use super::discovery::FcCandidate;
use super::lidar_exclusion::{drop_lidar_ports, is_rig_lidar_on_visor};
use super::*;
use crate::param_cache::ParamCache;
use crate::state::VehicleState;
use ados_hal_probe::lidar_port::LidarPort;

const CP2102: (u16, u16) = (0x10C4, 0xEA60);
const STM: (u16, u16) = (0x0483, 0x5740);

fn cand(name: &str, usb: Option<(u16, u16)>) -> FcCandidate {
    FcCandidate {
        name: name.into(),
        usb,
    }
}

/// An FC on native USB, a CP2102 and an onboard UART.
fn ports() -> Vec<FcCandidate> {
    vec![
        cand("/dev/ttyACM0", Some(STM)),
        cand("/dev/ttyUSB0", Some(CP2102)),
        cand("/dev/ttyS1", None),
    ]
}

fn names(c: &[FcCandidate]) -> Vec<&str> {
    c.iter().map(|c| c.name.as_str()).collect()
}

fn connection(serial_port: &str, profile: &str) -> std::sync::Arc<FcConnection> {
    let state = std::sync::Arc::new(Mutex::new(VehicleState::default()));
    let params = std::sync::Arc::new(Mutex::new(ParamCache::new(
        "/tmp/ados-lidar-exclusion-params.json",
    )));
    FcConnection::new(
        MavlinkConfig {
            serial_port: serial_port.into(),
            profile: profile.into(),
            ..MavlinkConfig::default()
        },
        state,
        params,
    )
}

#[test]
fn visor_excludes_a_cp2102_port_from_the_sweep() {
    assert!(is_rig_lidar_on_visor("visor", 0x10C4, 0xEA60));
    let kept = drop_lidar_ports("visor", &LidarPort::None, ports(), |_| false);
    assert_eq!(names(&kept), ["/dev/ttyACM0", "/dev/ttyS1"]);
}

#[test]
fn the_visor_rule_alone_keeps_a_drone_cp2102() {
    assert!(!is_rig_lidar_on_visor("drone", 0x10C4, 0xEA60));
    assert!(!is_rig_lidar_on_visor("visor", 0x1209, 0x5740));
}

#[test]
fn drone_excludes_a_cp2102_identified_as_the_declared_lidar() {
    let mut probed = Vec::new();
    let kept = drop_lidar_ports("drone", &LidarPort::Auto, ports(), |p| {
        probed.push(p.to_string());
        true
    });
    assert_eq!(names(&kept), ["/dev/ttyACM0", "/dev/ttyS1"]);
    assert_eq!(probed, ["/dev/ttyUSB0"], "only the CP2102 is probed");
}

#[test]
fn drone_keeps_a_cp2102_not_identified_as_the_lidar() {
    // A declared LiDAR, but this CP2102 does not answer as one (a telemetry
    // radio or an FC adapter).
    let kept = drop_lidar_ports("drone", &LidarPort::Auto, ports(), |_| false);
    assert_eq!(names(&kept), names(&ports()));
    // No LiDAR on the node: nothing is probed, nothing dropped.
    let kept = drop_lidar_ports("drone", &LidarPort::None, ports(), |_| {
        panic!("no LiDAR declared, no probe")
    });
    assert_eq!(names(&kept), names(&ports()));
}

#[test]
fn every_profile_excludes_the_configured_lidar_device() {
    for profile in ["drone", "ground-station", "visor"] {
        let rule = LidarPort::Device("/dev/ttyS1".into());
        let kept = drop_lidar_ports(profile, &rule, ports(), |_| panic!("no probe"));
        assert!(!names(&kept).contains(&"/dev/ttyS1"), "{profile}");
        assert!(names(&kept).contains(&"/dev/ttyACM0"), "{profile}");
    }
    // The configured path names a different node: a drone CP2102 stays.
    let rule = LidarPort::Device("/dev/ttyUSB7".into());
    let kept = drop_lidar_ports("drone", &rule, ports(), |_| panic!("no probe"));
    assert_eq!(names(&kept), names(&ports()));
}

#[cfg(unix)]
#[test]
fn the_configured_device_matches_through_its_by_id_link() {
    let dir = tempfile::tempdir().unwrap();
    let tty = dir.path().join("ttyUSB0");
    std::fs::write(&tty, b"").unwrap();
    let link = dir.path().join("usb-Silicon_Labs_CP2102-if00-port0");
    std::os::unix::fs::symlink(&tty, &link).unwrap();
    let rule = LidarPort::Device(link.to_string_lossy().into_owned());
    let tty = tty.to_string_lossy().into_owned();
    let kept = drop_lidar_ports("drone", &rule, vec![cand(&tty, Some(CP2102))], |_| false);
    assert!(kept.is_empty());
}

#[tokio::test]
async fn an_explicitly_configured_serial_port_is_kept() {
    for profile in ["visor", "drone"] {
        let c = connection("/dev/ttyUSB0", profile);
        let cands = c.without_lidar(c.candidate_ports()).await;
        assert_eq!(names(&cands), ["/dev/ttyUSB0"], "{profile}");
    }
}
