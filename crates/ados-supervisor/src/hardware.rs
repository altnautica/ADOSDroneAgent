//! Boot-time hardware detection: camera + WFB radio adapter.
//!
//! All reads are filesystem / subprocess probes, so the module compiles on any
//! host (the probes simply find nothing off a real SBC).
//!
//! Everything here runs BEFORE the supervisor signals readiness, which is why
//! every subprocess is bounded: a board that never reaches READY is a board
//! that never flies, and systemd's own start timeout is the only thing that
//! would eventually notice.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

/// Ceiling for the CSI camera probe.
///
/// `rpicam-hello --list-cameras` talks to the camera stack, and a camera whose
/// CSI link has not trained (a marginal ribbon, a half-seated connector — the
/// failure this probe exists to detect) can leave it blocked indefinitely.
/// Unbounded, that parked the whole pre-READY startup path on a wedged probe.
/// Generous enough for a cold libcamera enumeration on a Pi Zero-class SoC.
const CAMERA_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

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
    // `kill_on_drop` matters as much as the timeout: without it the timeout
    // path leaks the wedged probe, which keeps holding the camera stack the
    // video service is about to want.
    let child = Command::new("rpicam-hello")
        .arg("--list-cameras")
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(CAMERA_PROBE_TIMEOUT, child).await {
        Ok(Ok(out)) => String::from_utf8_lossy(&out.stdout).contains("Available cameras"),
        Ok(Err(_)) => false, // spawn error (binary absent off a Pi)
        Err(_) => {
            tracing::warn!(
                timeout_s = CAMERA_PROBE_TIMEOUT.as_secs(),
                "csi_camera_probe_timeout"
            );
            false
        }
    }
}

/// True if a WFB-ng capable adapter is on the USB bus.
///
/// Matched against the generated adapter table, which is the single source of
/// truth (`crates/ados-protocol/wfb-adapters.toml` → `wfb_tables`). This used
/// to be a three-entry hardcode: a second, silently-diverging table that made
/// a supported adapter — an RTL8812EU, or either TP-Link variant — read as "no
/// radio", so the supervisor never started the radio unit and the aircraft
/// came up with no link on hardware the rest of the stack fully supports.
///
/// The vendor deny-set is applied first, matching `ados-radio`'s classifier:
/// a management-WiFi chip that advertises monitor mode must never be taken for
/// an injection radio.
pub fn has_wfb_adapter() -> bool {
    enumerate_usb_ids()
        .iter()
        .any(|(vid, pid)| is_wfb_adapter_id(*vid, *pid))
}

/// Whether one USB `(vid, pid)` is a WFB-ng injection adapter, per the
/// generated table. Split out so the boot-detect classification is provable
/// without a USB bus.
pub fn is_wfb_adapter_id(vid: u16, pid: u16) -> bool {
    use ados_protocol::wfb_tables::{DENY_VID, WFB_COMPATIBLE};
    if DENY_VID.contains(&vid) {
        return false;
    }
    WFB_COMPATIBLE
        .iter()
        .any(|(v, p, _)| *v == vid && *p == pid)
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

    #[test]
    fn boot_detect_accepts_every_adapter_the_generated_table_declares() {
        // The regression: this classification was a three-entry hardcode
        // (a81a / 8812 / 881a), so an RTL8812EU or either TP-Link variant read
        // as "no radio" and the supervisor never started the radio unit on
        // hardware the rest of the stack fully supports. Asserting against the
        // generated table — not a second copy of it — is what stops a future
        // table entry from silently going undetected at boot again.
        for (vid, pid, label) in ados_protocol::wfb_tables::WFB_COMPATIBLE {
            assert!(
                is_wfb_adapter_id(*vid, *pid),
                "{label} ({vid:#06x}:{pid:#06x}) is a declared WFB adapter but \
                 boot detect does not recognise it"
            );
        }
    }

    #[test]
    fn boot_detect_refuses_a_denied_management_radio_and_an_unknown_id() {
        // A management-WiFi chip that advertises monitor mode must never be
        // taken for an injection radio: starting the radio unit on it takes
        // down the box's own management link and still gives no video.
        for vid in ados_protocol::wfb_tables::DENY_VID {
            assert!(!is_wfb_adapter_id(*vid, 0x8812));
        }
        // A plain USB-serial bridge is not a radio.
        assert!(!is_wfb_adapter_id(0x1A86, 0x7523));
    }
}
