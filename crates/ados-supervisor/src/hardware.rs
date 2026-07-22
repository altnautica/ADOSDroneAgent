//! Boot-time hardware detection: camera + WFB radio adapter.
//!
//! Mirrors the Python supervisor's detect pass. All reads are filesystem /
//! subprocess probes, so the module compiles on any host (the probes simply
//! find nothing off a real SBC).

use std::path::Path;

use tokio::process::Command;

/// True if a video node exists or a CSI camera is present.
pub async fn has_camera() -> bool {
    if video_node_present() {
        return true;
    }
    csi_camera_present().await
}

/// `/dev/video[0-9]+` present.
pub fn video_node_present() -> bool {
    dev_nodes_present(&["video"])
}

async fn csi_camera_present() -> bool {
    match Command::new("rpicam-hello")
        .arg("--list-cameras")
        .output()
        .await
    {
        Ok(out) => String::from_utf8_lossy(&out.stdout).contains("Available cameras"),
        Err(_) => false,
    }
}

/// True if an RTL8812-family WFB adapter is on the USB bus (boot detect set:
/// the same VID/PID triple the Python `_check_wfb_adapter` uses).
pub fn has_wfb_adapter() -> bool {
    const WFB_IDS: [(u16, u16); 3] = [(0x0BDA, 0xA81A), (0x0BDA, 0x8812), (0x0BDA, 0x881A)];
    enumerate_usb_ids().iter().any(|id| WFB_IDS.contains(id))
}

/// Read `(idVendor, idProduct)` for every device under `/sys/bus/usb/devices`.
pub fn enumerate_usb_ids() -> Vec<(u16, u16)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/sys/bus/usb/devices") else {
        return out;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if let (Some(v), Some(pid)) = (
            read_hex16(&p.join("idVendor")),
            read_hex16(&p.join("idProduct")),
        ) {
            out.push((v, pid));
        }
    }
    out
}

fn read_hex16(p: &Path) -> Option<u16> {
    let s = std::fs::read_to_string(p).ok()?;
    u16::from_str_radix(s.trim(), 16).ok()
}

/// True if any `/dev/<prefix>[0-9]+` node exists for one of `prefixes`.
pub fn dev_nodes_present(prefixes: &[&str]) -> bool {
    let Ok(rd) = std::fs::read_dir("/dev") else {
        return false;
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let n = name.to_string_lossy();
        for pre in prefixes {
            if let Some(rest) = n.strip_prefix(pre) {
                if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                    return true;
                }
            }
        }
    }
    false
}

/// The USB-serial tty inventory: every `/dev/tty{ACM,USB}<n>` node with its
/// backing USB `(idVendor, idProduct)` resolved from sysfs (`None` for a node
/// with no USB ancestor, or off-Linux). Sorted by name so the snapshot is
/// stable across polls. This is the node-level view the hot-plug classifier
/// needs: a class-wide "any tty exists" bool cannot tell an RC module's bridge
/// apart from the flight controller sitting next to it.
pub fn serial_tty_nodes() -> Vec<(String, Option<(u16, u16)>)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/dev") else {
        return out;
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_indexed_serial_node(&name) {
            let usb = tty_usb_id(&name);
            out.push((name, usb));
        }
    }
    out.sort();
    out
}

/// True for `ttyACM<n>` / `ttyUSB<n>` (an index is required, so `ttyACM` bare
/// or `ttyUSBx` never match).
fn is_indexed_serial_node(name: &str) -> bool {
    for pre in ["ttyACM", "ttyUSB"] {
        if let Some(rest) = name.strip_prefix(pre) {
            return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit());
        }
    }
    false
}

/// Resolve a tty node's backing USB id: `/sys/class/tty/<node>/device` points
/// at the USB *interface*; `idVendor`/`idProduct` live on an ancestor USB
/// device directory, so climb parents until one carries both files.
fn tty_usb_id(node: &str) -> Option<(u16, u16)> {
    let start = std::fs::canonicalize(format!("/sys/class/tty/{node}/device")).ok()?;
    usb_id_above(&start)
}

/// Walk up from `start` looking for a directory carrying `idVendor` +
/// `idProduct` (bounded, and never climbing out of /sys). Split from
/// [`tty_usb_id`] so a fake directory layout can exercise it in a test.
fn usb_id_above(start: &Path) -> Option<(u16, u16)> {
    let mut cur = start.to_path_buf();
    for _ in 0..6 {
        if let (Some(vid), Some(pid)) = (
            read_hex16(&cur.join("idVendor")),
            read_hex16(&cur.join("idProduct")),
        ) {
            return Some((vid, pid));
        }
        let parent = cur.parent()?.to_path_buf();
        if parent == Path::new("/sys") || parent == Path::new("/") {
            return None;
        }
        cur = parent;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_hex16_parses_sysfs_form() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("idVendor");
        std::fs::write(&f, "0bda\n").unwrap();
        assert_eq!(read_hex16(&f), Some(0x0BDA));
        assert_eq!(read_hex16(&dir.path().join("missing")), None);
    }

    #[test]
    fn indexed_serial_node_requires_a_numeric_index() {
        assert!(is_indexed_serial_node("ttyACM0"));
        assert!(is_indexed_serial_node("ttyUSB12"));
        assert!(!is_indexed_serial_node("ttyACM"));
        assert!(!is_indexed_serial_node("ttyUSBx"));
        assert!(!is_indexed_serial_node("ttyprintk"));
        assert!(!is_indexed_serial_node("ttyS0"));
    }

    #[test]
    fn usb_id_above_climbs_to_the_device_dir() {
        // The interface dir has no id files; the parent (the USB device) does.
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("usbdev");
        let iface = device.join("iface");
        std::fs::create_dir_all(&iface).unwrap();
        std::fs::write(device.join("idVendor"), "1a86\n").unwrap();
        std::fs::write(device.join("idProduct"), "7523\n").unwrap();
        assert_eq!(usb_id_above(&iface), Some((0x1A86, 0x7523)));
        // No USB ancestor anywhere -> None.
        let bare = dir.path().join("soc-uart");
        std::fs::create_dir_all(&bare).unwrap();
        assert_eq!(usb_id_above(&bare), None);
    }
}
