//! Sensor-hub USB presence: which `/dev/ttyACM*` node, if any, is the sensor
//! hub, the `sensor_hub` object of `board.json`, and the
//! `/run/ados/sensor-hub-present` marker the hub service and the capability
//! surface key on.
//!
//! The sysfs walk takes the `/sys/class/tty` root as a parameter so a fake
//! layout drives it in tests; `ados-sensorhub` uses the same walk for its
//! `transport: auto` resolution.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::bindings::{parse_vid_pid, SensorHubBinding, SENSOR_HUB_VID_PID};
use crate::probe::serial::{has_indexed_prefix, resolve_usb_id};

/// The sysfs class directory every tty node exposes a link under.
pub const TTY_CLASS_DIR: &str = "/sys/class/tty";

/// One CDC-ACM node and the USB device behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbTty {
    /// `/dev/ttyACM<n>`.
    pub device: String,
    pub vid: u16,
    pub pid: u16,
}

/// Every `ttyACM<n>` under a `/sys/class/tty`-shaped root that resolves to a
/// USB device, in node-index order (`ttyACM2` before `ttyACM10`). An
/// unreadable root yields an empty list.
pub fn usb_acm_ttys(tty_root: &Path) -> Vec<UsbTty> {
    let Ok(rd) = std::fs::read_dir(tty_root) else {
        return Vec::new();
    };
    let mut out: Vec<(u32, UsbTty)> = rd
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !has_indexed_prefix(&name, "ttyACM") {
                return None;
            }
            let index = name["ttyACM".len()..].parse::<u32>().ok()?;
            let id = resolve_usb_id(&entry.path().join("device"))?;
            Some((
                index,
                UsbTty {
                    device: format!("/dev/{name}"),
                    vid: id.vid,
                    pid: id.pid,
                },
            ))
        })
        .collect();
    out.sort_by_key(|(index, _)| *index);
    out.into_iter().map(|(_, tty)| tty).collect()
}

/// The first `/dev/ttyACM*` node backed by USB `vid:pid`.
pub fn find_acm_by_usb_id(tty_root: &Path, vid: u16, pid: u16) -> Option<String> {
    usb_acm_ttys(tty_root)
        .into_iter()
        .find(|t| t.vid == vid && t.pid == pid)
        .map(|t| t.device)
}

/// The `sensor_hub` object of `board.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SensorHubPresence {
    pub present: bool,
    pub device: Option<String>,
    /// Lower-case `vvvv:pppp`.
    pub vid_pid: String,
}

/// Resolve the hub presence from the board's binding and the enumerated ACM
/// nodes (pure). A board that declares a hub always gets an object
/// (`present: false` while unplugged); a board that declares none gets one
/// only when a hub is actually attached.
pub fn detect(binding: Option<&SensorHubBinding>, ttys: &[UsbTty]) -> Option<SensorHubPresence> {
    let declared = binding
        .map(|b| b.usb_vid_pid.trim().to_ascii_lowercase())
        .unwrap_or_else(|| SENSOR_HUB_VID_PID.to_string());
    let (vid_pid, (vid, pid)) = match parse_vid_pid(&declared) {
        Some(ids) => (declared, ids),
        None => {
            tracing::warn!(
                usb_vid_pid = %declared,
                "sensor hub binding VID:PID unparseable; using the firmware default"
            );
            let ids = parse_vid_pid(SENSOR_HUB_VID_PID)?;
            (SENSOR_HUB_VID_PID.to_string(), ids)
        }
    };
    let device = ttys
        .iter()
        .find(|t| t.vid == vid && t.pid == pid)
        .map(|t| t.device.clone());
    if device.is_none() && binding.is_none() {
        return None;
    }
    Some(SensorHubPresence {
        present: device.is_some(),
        device,
        vid_pid,
    })
}

/// The presence marker path (`ADOS_RUN_DIR` aware).
pub fn marker_path() -> PathBuf {
    ados_protocol::paths::run_path("sensor-hub-present")
}

/// Create the marker when the hub is present, remove it otherwise.
pub fn sync_marker(path: &Path, present: bool) -> std::io::Result<()> {
    if present {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, b"")
    } else {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

/// Mirror a published presence onto the marker (best-effort, logged); returns
/// whether the hub is present.
pub fn publish_presence(presence: Option<&SensorHubPresence>) -> bool {
    let present = presence.is_some_and(|h| h.present);
    if let Err(e) = sync_marker(&marker_path(), present) {
        tracing::warn!(error = %e, "sensor hub presence marker not updated");
    }
    present
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake `/sys/class/tty` node: `<root>/<node>/device` is the USB
    /// interface dir, its parent carries `idVendor` / `idProduct`.
    fn fake_tty(root: &Path, node: &str, vid: &str, pid: &str) {
        let usb_dev = root.join(node);
        std::fs::create_dir_all(usb_dev.join("device")).unwrap();
        std::fs::write(usb_dev.join("idVendor"), format!("{vid}\n")).unwrap();
        std::fs::write(usb_dev.join("idProduct"), format!("{pid}\n")).unwrap();
    }

    #[test]
    fn the_walk_finds_the_hub_by_vid_pid_in_index_order() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fake_tty(root, "ttyACM0", "1209", "5741");
        fake_tty(root, "ttyACM10", "1209", "ad05");
        fake_tty(root, "ttyACM2", "1209", "ad05");
        fake_tty(root, "ttyUSB0", "1209", "ad05");
        std::fs::create_dir_all(root.join("ttyS0")).unwrap();

        let ttys = usb_acm_ttys(root);
        let devices: Vec<&str> = ttys.iter().map(|t| t.device.as_str()).collect();
        assert_eq!(devices, ["/dev/ttyACM0", "/dev/ttyACM2", "/dev/ttyACM10"]);
        assert_eq!(
            find_acm_by_usb_id(root, 0x1209, 0xAD05).as_deref(),
            Some("/dev/ttyACM2")
        );
        assert_eq!(find_acm_by_usb_id(root, 0x0483, 0x5740), None);
        assert!(usb_acm_ttys(&root.join("missing")).is_empty());
    }

    #[test]
    fn presence_follows_the_binding_and_the_bus() {
        let hub = UsbTty {
            device: "/dev/ttyACM1".into(),
            vid: 0x1209,
            pid: 0xAD05,
        };
        let fc = UsbTty {
            device: "/dev/ttyACM0".into(),
            vid: 0x1209,
            pid: 0x5741,
        };
        // No binding, no hub: absent.
        assert_eq!(detect(None, std::slice::from_ref(&fc)), None);
        // No binding, hub attached: present.
        let p = detect(None, &[fc.clone(), hub.clone()]).unwrap();
        assert!(p.present);
        assert_eq!(p.device.as_deref(), Some("/dev/ttyACM1"));
        assert_eq!(p.vid_pid, "1209:ad05");
        // Declared but unplugged: an object reading not present.
        let binding = SensorHubBinding::default();
        let p = detect(Some(&binding), std::slice::from_ref(&fc)).unwrap();
        assert!(!p.present);
        assert_eq!(p.device, None);
        // A declared custom id matches that id only.
        let custom = SensorHubBinding {
            usb_vid_pid: "1209:5741".into(),
            board: None,
        };
        let p = detect(Some(&custom), &[fc, hub]).unwrap();
        assert_eq!(p.device.as_deref(), Some("/dev/ttyACM0"));
        assert_eq!(p.vid_pid, "1209:5741");
    }

    #[test]
    fn the_marker_is_created_and_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("run").join("sensor-hub-present");
        sync_marker(&marker, true).unwrap();
        assert!(marker.exists());
        sync_marker(&marker, true).unwrap();
        sync_marker(&marker, false).unwrap();
        assert!(!marker.exists());
        sync_marker(&marker, false).unwrap();
    }
}
