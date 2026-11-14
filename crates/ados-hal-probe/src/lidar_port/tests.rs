use super::*;
use std::path::Path;

const CP2102: Option<(u16, u16)> = Some(LIDAR_USB_ID);
const STM: Option<(u16, u16)> = Some((0x0483, 0x5740));

fn record(agent_id: Option<&str>, value: &rmpv::Value) -> serde_json::Value {
    use base64::Engine;
    let mut bytes = Vec::new();
    rmpv::encode::write_value(&mut bytes, value).unwrap();
    let mut r = serde_json::json!({
        "plugin_id": LIDAR_PLUGIN_ID,
        "key": LIDAR_CONFIG_KEY,
        "value": base64::engine::general_purpose::STANDARD.encode(bytes),
    });
    if let Some(a) = agent_id {
        r["agent_id"] = a.into();
    }
    r
}

fn lidar_cfg(device: &str) -> rmpv::Value {
    rmpv::Value::Map(vec![(
        "rplidar".into(),
        rmpv::Value::Map(vec![
            ("device".into(), device.into()),
            ("baud".into(), 460_800.into()),
        ]),
    )])
}

fn write_json(path: &Path, v: &serde_json::Value) {
    std::fs::write(path, serde_json::to_vec(v).unwrap()).unwrap();
}

fn rig(dir: &Path, kinds: &[&str]) -> std::path::PathBuf {
    let sensors: Vec<_> = kinds
        .iter()
        .enumerate()
        .map(|(i, k)| serde_json::json!({"id": format!("s{i}"), "kind": k, "role": k}))
        .collect();
    let path = dir.join("rig.json");
    write_json(
        &path,
        &serde_json::json!({"rig_id": "r", "sensors": sensors}),
    );
    path
}

#[test]
fn a_configured_device_path_is_the_lidar_on_any_usb_id() {
    let rule = LidarPort::from_parts(Some("/dev/ttyS3"), false);
    assert_eq!(rule, LidarPort::Device("/dev/ttyS3".into()));
    assert!(rule.identifies("/dev/ttyS3", None, |_| panic!("no probe")));
    assert!(!rule.identifies("/dev/ttyUSB0", CP2102, |_| panic!("no probe")));
}

#[cfg(unix)]
#[test]
fn a_by_id_device_matches_its_resolved_tty() {
    let dir = tempfile::tempdir().unwrap();
    let tty = dir.path().join("ttyUSB0");
    std::fs::write(&tty, b"").unwrap();
    let link = dir.path().join("usb-Silicon_Labs_CP2102-if00-port0");
    std::os::unix::fs::symlink(&tty, &link).unwrap();
    let rule = LidarPort::Device(link.to_string_lossy().into_owned());
    assert!(rule.identifies(&tty.to_string_lossy(), CP2102, |_| false));
}

#[test]
fn auto_with_a_declared_lidar_needs_a_cp2102_that_confirms() {
    for device in [None, Some("auto"), Some(" auto "), Some("")] {
        assert_eq!(LidarPort::from_parts(device, true), LidarPort::Auto);
        assert_eq!(LidarPort::from_parts(device, false), LidarPort::None);
    }
    let rule = LidarPort::Auto;
    assert!(rule.identifies("/dev/ttyUSB0", CP2102, |p| p == "/dev/ttyUSB0"));
    assert!(!rule.identifies("/dev/ttyUSB0", CP2102, |_| false));
    assert!(!rule.identifies("/dev/ttyACM0", STM, |_| panic!("no probe")));
    assert!(!rule.identifies("/dev/ttyS0", None, |_| panic!("no probe")));
}

#[test]
fn a_network_lidar_or_none_claims_no_port() {
    let tcp = LidarPort::from_parts(Some("tcp:127.0.0.1:20108"), true);
    assert_eq!(tcp, LidarPort::None);
    assert!(!tcp.identifies("/dev/ttyUSB0", CP2102, |_| panic!("no probe")));
}

#[test]
fn load_reads_the_plugin_config_and_the_rig() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("plugin-config.json");
    let with_lidar = rig(dir.path(), &["camera", "lidar"]);

    // No config: the device defaults to auto, the rig declares a LiDAR.
    assert_eq!(LidarPort::load_from(&with_lidar, &cfg), LidarPort::Auto);

    // A configured path.
    write_json(
        &cfg,
        &serde_json::json!([record(None, &lidar_cfg("/dev/serial/by-id/lidar"))]),
    );
    assert_eq!(
        LidarPort::load_from(&with_lidar, &cfg),
        LidarPort::Device("/dev/serial/by-id/lidar".into())
    );

    // The per-drone record wins over the global one.
    write_json(
        &cfg,
        &serde_json::json!([
            record(None, &lidar_cfg("/dev/ttyUSB3")),
            record(Some("drone-1"), &lidar_cfg("auto")),
        ]),
    );
    assert_eq!(LidarPort::load_from(&with_lidar, &cfg), LidarPort::Auto);

    // Another plugin's or another key's record is not the LiDAR config.
    let mut other = record(None, &lidar_cfg("/dev/ttyUSB3"));
    other["plugin_id"] = "com.example.other".into();
    write_json(&cfg, &serde_json::json!([other]));
    assert_eq!(LidarPort::load_from(&with_lidar, &cfg), LidarPort::Auto);

    // A rig without a LiDAR and no device path: nothing is the LiDAR.
    let dir2 = tempfile::tempdir().unwrap();
    let no_lidar = rig(dir2.path(), &["camera", "imu"]);
    assert_eq!(LidarPort::load_from(&no_lidar, &cfg), LidarPort::None);
    assert_eq!(
        LidarPort::load_from(&dir2.path().join("missing.json"), &cfg),
        LidarPort::None
    );
}

#[test]
fn descriptor_is_found_anywhere_in_the_reply() {
    assert!(contains_info_descriptor(&INFO_DESCRIPTOR));
    let mut noisy = vec![0x00, 0xA5, 0x13];
    noisy.extend_from_slice(&INFO_DESCRIPTOR);
    noisy.extend_from_slice(&[0x41; 20]);
    assert!(contains_info_descriptor(&noisy));
    assert!(!contains_info_descriptor(&INFO_DESCRIPTOR[..6]));
    assert!(!contains_info_descriptor(b"\xFE\x09\x00\x01\x01"));
    assert!(RplidarProbe::Answered.is_lidar());
    assert!(RplidarProbe::Busy.is_lidar());
    assert!(!RplidarProbe::Silent.is_lidar());
}

#[cfg(target_os = "linux")]
mod pty {
    use super::super::probe::probe_with_timeout;
    use super::*;
    use std::io::{Read, Write};
    use std::time::Duration;

    /// A pseudo-terminal: the probe opens the slave path, the test plays the
    /// device on the master.
    fn pty() -> (std::fs::File, std::os::fd::OwnedFd, String) {
        let p = nix::pty::openpty(None, None).unwrap();
        let path = nix::unistd::ttyname(&p.slave).unwrap();
        (
            std::fs::File::from(p.master),
            p.slave,
            path.to_string_lossy().into_owned(),
        )
    }

    #[test]
    fn a_device_answering_get_info_is_the_lidar() {
        let (mut master, _slave, path) = pty();
        let device = std::thread::spawn(move || {
            let mut seen = Vec::new();
            let mut buf = [0u8; 64];
            while !seen.windows(2).any(|w| w == [0xA5, 0x50]) {
                let n = master.read(&mut buf).unwrap();
                seen.extend_from_slice(&buf[..n]);
            }
            master.write_all(&INFO_DESCRIPTOR).unwrap();
            master.write_all(&[0x41; 20]).unwrap();
            // Hand the master back: closing it hangs the slave up, and Linux
            // drops input the probe has not read yet.
            (seen, master)
        });
        assert_eq!(
            probe_with_timeout(&path, Duration::from_secs(2)),
            RplidarProbe::Answered
        );
        let (seen, _master) = device.join().unwrap();
        assert!(seen.starts_with(&[0xA5, 0x25]), "STOP first: {seen:02x?}");
    }

    #[test]
    fn a_silent_port_is_not_the_lidar() {
        let (_master, _slave, path) = pty();
        assert_eq!(
            probe_with_timeout(&path, Duration::from_millis(50)),
            RplidarProbe::Silent
        );
    }

    #[test]
    fn a_port_another_process_holds_is_busy_and_left_alone() {
        use nix::fcntl::{Flock, FlockArg};
        let (_master, _slave, path) = pty();
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let _lock = Flock::lock(held, FlockArg::LockExclusiveNonblock).unwrap();
        assert_eq!(probe_rplidar(&path), RplidarProbe::Busy);
    }

    #[test]
    fn a_missing_port_is_silent() {
        assert_eq!(probe_rplidar("/dev/ados-no-such-tty"), RplidarProbe::Silent);
    }
}
