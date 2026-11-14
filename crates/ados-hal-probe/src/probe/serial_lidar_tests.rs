//! The serial inventory scores the rig's LiDAR port 0 and keeps every other
//! CP2102 an FC candidate.

use super::*;

/// A fake `/sys/class/tty` with an STM native-USB FC on `ttyACM0` and a
/// CP2102 bridge on `ttyUSB0`.
fn layout() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    for (node, vid, pid) in [("ttyACM0", "0483", "5740"), ("ttyUSB0", "10c4", "ea60")] {
        let dev = tmp.path().join(node).join("device");
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::write(dev.join("idVendor"), format!("{vid}\n")).unwrap();
        std::fs::write(dev.join("idProduct"), format!("{pid}\n")).unwrap();
    }
    tmp
}

fn score(ports: &[SerialPort], path: &str) -> u8 {
    ports.iter().find(|p| p.path == path).unwrap().fc_score
}

#[test]
fn a_cp2102_confirmed_as_the_declared_lidar_scores_zero() {
    let tmp = layout();
    let mut asked = Vec::new();
    let ports = enumerate_from_root(tmp.path(), None, &LidarPort::Auto, &mut |p| {
        asked.push(p.to_string());
        true
    });
    assert_eq!(
        asked,
        vec!["/dev/ttyUSB0".to_string()],
        "only the CP2102 is probed"
    );
    assert_eq!(score(&ports, "/dev/ttyUSB0"), 0);
    assert_eq!(score(&ports, "/dev/ttyACM0"), 90);
    // Still in the inventory, with its USB id, sorted last.
    assert_eq!(ports.last().unwrap().path, "/dev/ttyUSB0");
    assert_eq!(
        ports.last().unwrap().usb,
        Some(UsbId {
            vid: 0x10C4,
            pid: 0xEA60
        })
    );
}

#[test]
fn an_unconfirmed_cp2102_stays_an_fc_candidate() {
    let tmp = layout();
    let ports = enumerate_from_root(tmp.path(), None, &LidarPort::Auto, &mut |_| false);
    assert_eq!(score(&ports, "/dev/ttyUSB0"), 70);
    let ports = enumerate_from_root(tmp.path(), None, &LidarPort::None, &mut |_| {
        panic!("no LiDAR declared, no probe")
    });
    assert_eq!(score(&ports, "/dev/ttyUSB0"), 70);
}

#[test]
fn the_configured_lidar_device_scores_zero_without_a_probe() {
    let tmp = layout();
    let rule = LidarPort::Device("/dev/ttyUSB0".into());
    let ports = enumerate_from_root(tmp.path(), None, &rule, &mut |_| panic!("no probe"));
    assert_eq!(score(&ports, "/dev/ttyUSB0"), 0);
    assert_eq!(score(&ports, "/dev/ttyACM0"), 90);
}

#[test]
fn the_pinned_crsf_port_is_never_probed_for_the_lidar() {
    let tmp = layout();
    let ports = enumerate_from_root(
        tmp.path(),
        Some("/dev/ttyUSB0"),
        &LidarPort::Auto,
        &mut |_| panic!("the RC module port is not probed"),
    );
    assert_eq!(score(&ports, "/dev/ttyUSB0"), 0);
}
