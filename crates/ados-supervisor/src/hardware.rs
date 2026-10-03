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

/// One identity per `/dev/video<n>` node: the node name plus, for a USB
/// camera, the USB device it sits on (`bus-dev vid:pid`). The device number
/// changes on every re-enumeration, so a camera swapped or replugged between
/// two polls reads as a different identity.
pub fn video_node_ids() -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    let Ok(rd) = std::fs::read_dir("/dev") else {
        return out;
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix("video") else {
            continue;
        };
        if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let usb = std::fs::canonicalize(format!("/sys/class/video4linux/{name}/device"))
            .ok()
            .and_then(|start| usb_device_above(&start));
        out.insert(match usb {
            Some(dev) => format!("{name} {}", dev.identity()),
            None => name,
        });
    }
    out
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
    enumerate_usb_devices()
        .into_iter()
        .map(|d| (d.vid, d.pid))
        .collect()
}

/// One enumerated USB device: its ids and its bus/device numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDevice {
    pub vid: u16,
    pub pid: u16,
    /// `busnum-devnum`; the device number is reassigned on every
    /// re-enumeration. Empty when the kernel did not expose it.
    pub bus_dev: String,
}

impl UsbDevice {
    /// `bus-dev vid:pid`: distinct for a replugged or swapped device.
    pub fn identity(&self) -> String {
        format!("{} {:04x}:{:04x}", self.bus_dev, self.vid, self.pid)
    }
}

/// Every device under `/sys/bus/usb/devices` with ids and bus position.
pub fn enumerate_usb_devices() -> Vec<UsbDevice> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/sys/bus/usb/devices") else {
        return out;
    };
    for entry in rd.flatten() {
        if let Some(dev) = usb_device_at(&entry.path()) {
            out.push(dev);
        }
    }
    out
}

/// The USB device whose sysfs directory is `dir`, when it carries ids.
fn usb_device_at(dir: &Path) -> Option<UsbDevice> {
    let vid = read_hex16(&dir.join("idVendor"))?;
    let pid = read_hex16(&dir.join("idProduct"))?;
    let num = |f: &str| {
        std::fs::read_to_string(dir.join(f))
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    Some(UsbDevice {
        vid,
        pid,
        bus_dev: format!("{}-{}", num("busnum"), num("devnum")),
    })
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

/// One USB-serial tty node with the USB device backing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerialNode {
    pub name: String,
    /// The backing USB device, `None` for a node with no USB ancestor.
    pub usb: Option<UsbDevice>,
}

impl SerialNode {
    /// Node name plus the USB device identity, so a replug that lands on the
    /// same node name still reads as a different device.
    pub fn identity(&self) -> String {
        match &self.usb {
            Some(dev) => format!("{} {}", self.name, dev.identity()),
            None => self.name.clone(),
        }
    }
}

/// The USB-serial tty inventory: every `/dev/tty{ACM,USB}<n>` node with its
/// backing USB device resolved from sysfs (`None` for a node with no USB
/// ancestor, or off-Linux). Sorted by name so the snapshot is stable across
/// polls. This is the node-level view the hot-plug classifier needs: a
/// class-wide "any tty exists" bool cannot tell an RC module's bridge apart
/// from the flight controller sitting next to it.
pub fn serial_tty_nodes() -> Vec<SerialNode> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/dev") else {
        return out;
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_indexed_serial_node(&name) {
            let usb = tty_usb_device(&name);
            out.push(SerialNode { name, usb });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
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

/// Resolve a tty node's backing USB device: `/sys/class/tty/<node>/device`
/// points at the USB *interface*; the ids live on an ancestor USB device
/// directory, so climb parents until one carries them.
fn tty_usb_device(node: &str) -> Option<UsbDevice> {
    let start = std::fs::canonicalize(format!("/sys/class/tty/{node}/device")).ok()?;
    usb_device_above(&start)
}

/// Walk up from `start` looking for a directory carrying `idVendor` +
/// `idProduct` (bounded, and never climbing out of /sys). Split from
/// [`tty_usb_device`] so a fake directory layout can exercise it in a test.
fn usb_device_above(start: &Path) -> Option<UsbDevice> {
    let mut cur = start.to_path_buf();
    for _ in 0..6 {
        if let Some(dev) = usb_device_at(&cur) {
            return Some(dev);
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
    fn usb_device_above_climbs_to_the_device_dir() {
        // The interface dir has no id files; the parent (the USB device) does.
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("usbdev");
        let iface = device.join("iface");
        std::fs::create_dir_all(&iface).unwrap();
        std::fs::write(device.join("idVendor"), "1a86\n").unwrap();
        std::fs::write(device.join("idProduct"), "7523\n").unwrap();
        std::fs::write(device.join("busnum"), "1\n").unwrap();
        std::fs::write(device.join("devnum"), "4\n").unwrap();
        let dev = usb_device_above(&iface).unwrap();
        assert_eq!((dev.vid, dev.pid), (0x1A86, 0x7523));
        assert_eq!(dev.identity(), "1-4 1a86:7523");
        // The same device re-enumerated gets a new device number.
        std::fs::write(device.join("devnum"), "5\n").unwrap();
        assert_ne!(usb_device_above(&iface).unwrap().identity(), dev.identity());
        // No USB ancestor anywhere -> None.
        let bare = dir.path().join("soc-uart");
        std::fs::create_dir_all(&bare).unwrap();
        assert_eq!(usb_device_above(&bare), None);
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
