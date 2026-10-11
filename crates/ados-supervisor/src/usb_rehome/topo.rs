//! USB-topology resolution + the never-rehome-the-control-interface guard.
//!
//! `resolve_usb_topo` walks `/sys` from a netdev to the USB device node backing
//! it (the same parent-walk the radio adapter selection uses). `guard_verdict`
//! is a pure, fail-closed comparison that refuses a rehome that could disturb
//! the operator's management link. The walk is Linux-only; the node-name
//! classifier and the guard are pure and unit-tested on every host.

#[cfg(any(target_os = "linux", test))]
use std::path::Path;

/// The USB topology of a netdev: the bind id (the device node basename written
/// to `/sys/bus/usb/drivers/usb/{unbind,bind}`) and the USB device-node
/// ancestors above it, up to the root hub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbTopo {
    /// e.g. "1-1" or "2-1.4" — the device node, the unbind/bind target.
    pub bind_id: String,
    /// Ancestor USB device nodes above the device (hubs + the root hub),
    /// nearest-first, e.g. ["1-1", "usb1"].
    pub ancestors: Vec<String>,
}

/// The control (management) interface's USB relationship, for the guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlPath {
    /// No default route — the control interface cannot be identified.
    NoRoute,
    /// The control interface is not USB-backed (wired Ethernet, onboard
    /// SDIO/PCIe WiFi). A USB unbind provably cannot touch it.
    NonUsb,
    /// The control interface is USB-backed, with this topology.
    Usb(UsbTopo),
}

/// The guard's verdict for a candidate rehome. Anything but `Allow` refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardVerdict {
    Allow,
    /// The target IS the control interface's device.
    BlockIsControl,
    /// The control interface hangs off the target (a hub) — unbinding the
    /// target would re-enumerate the control link.
    BlockSharesBranch,
    /// No default route: cannot prove the rehome is safe. Fail-closed.
    BlockUnprovable,
}

impl GuardVerdict {
    /// The bland reason string for the event (None when Allow).
    pub fn reason(self) -> Option<&'static str> {
        match self {
            GuardVerdict::Allow => None,
            GuardVerdict::BlockIsControl => Some("is_control"),
            GuardVerdict::BlockSharesBranch => Some("shares_branch"),
            GuardVerdict::BlockUnprovable => Some("unprovable"),
        }
    }
}

/// True when a `/sys` directory basename denotes a USB device node: a root hub
/// (`usbN`) or a device/hub node (`<bus>-<port>[.<port>...]`, e.g. `1-1`,
/// `2-1.4.3`). The host controller (e.g. `xhci-hcd.0.auto`, a PCI address) and
/// the interface node (`1-1:1.0`, has a colon) are NOT device nodes. Pure.
pub fn is_usb_node_name(name: &str) -> bool {
    if let Some(rest) = name.strip_prefix("usb") {
        return !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit());
    }
    if !name.contains('-') {
        return false;
    }
    let mut seen_dash = false;
    for b in name.bytes() {
        match b {
            b'-' => seen_dash = true,
            b'0'..=b'9' | b'.' => {}
            _ => return false,
        }
    }
    seen_dash
}

/// The fail-closed guard. A soft USB unbind re-enumerates the target device and
/// its descendants only, so the control link is at risk iff it IS the target or
/// hangs beneath it. Without a default route the control path is unknown and the
/// rehome is refused. Pure.
pub fn guard_verdict(target: &UsbTopo, control: &ControlPath) -> GuardVerdict {
    match control {
        ControlPath::NoRoute => GuardVerdict::BlockUnprovable,
        // A USB unbind cannot touch a non-USB management link.
        ControlPath::NonUsb => GuardVerdict::Allow,
        ControlPath::Usb(c) => {
            if c.bind_id == target.bind_id {
                GuardVerdict::BlockIsControl
            } else if c.ancestors.contains(&target.bind_id) {
                // The control device descends from the target (a hub): unbinding
                // the target would re-enumerate the control link too.
                GuardVerdict::BlockSharesBranch
            } else {
                GuardVerdict::Allow
            }
        }
    }
}

/// Evaluate the guard against a SET of protected paths (the management link AND
/// the radio AND the FC), returning the first non-`Allow` verdict. Fail-closed:
/// an empty set is `BlockUnprovable` (nothing proven safe). Used for the camera
/// hub-reset path, where re-enumerating a shared hub must not disturb any of the
/// protected devices. Pure.
pub fn guard_verdict_multi(target: &UsbTopo, protected: &[ControlPath]) -> GuardVerdict {
    if protected.is_empty() {
        return GuardVerdict::BlockUnprovable;
    }
    for c in protected {
        let v = guard_verdict(target, c);
        if v != GuardVerdict::Allow {
            return v;
        }
    }
    GuardVerdict::Allow
}

/// True when unbinding `target` (a leaf device, e.g. the camera) provably cannot
/// disturb any protected USB device: the target is none of them and none of them
/// descends from it. A soft unbind re-enumerates only the target + its
/// descendants, so a true leaf is always safe regardless of the management link
/// (unlike a hub reset, this does not fail-closed on an unknown control path).
/// Pure.
pub fn target_safe_as_leaf(target: &UsbTopo, protected_usb: &[UsbTopo]) -> bool {
    !protected_usb
        .iter()
        .any(|p| p.bind_id == target.bind_id || p.ancestors.contains(&target.bind_id))
}

/// Split a USB device-node bind id into its parent hub node + downstream port
/// number. `"1-1.1" -> ("1-1", 1)`, `"2-1.4.3" -> ("2-1.4", 3)`, and a root-port
/// device `"1-1" -> ("usb1", 1)`. Drives the per-port reset + the `disable`
/// attribute path. Pure.
pub fn hub_and_port(bind_id: &str) -> Option<(String, u32)> {
    if let Some(idx) = bind_id.rfind('.') {
        let hub = bind_id[..idx].to_string();
        let port = bind_id[idx + 1..].parse::<u32>().ok()?;
        if hub.is_empty() {
            return None;
        }
        return Some((hub, port));
    }
    // Root-port device: "<bus>-<port>" hangs directly off root hub "usb<bus>".
    let (bus, port) = bind_id.split_once('-')?;
    let busn = bus.parse::<u32>().ok()?;
    let portn = port.parse::<u32>().ok()?;
    Some((format!("usb{}", busn), portn))
}

/// The sysfs `disable` attribute for a hub's downstream port, e.g.
/// `1-1-port1/disable`. Toggling it (1 then 0) re-enables just that one port,
/// re-enumerating only the device on it. Pure (path only).
pub fn port_disable_rel(hub: &str, port: u32) -> String {
    format!("{}-port{}/disable", hub, port)
}

/// Resolve a video device's USB topology, or `None` when it is not USB-backed.
/// `/dev/video0` (or `video0`) → `/sys/class/video4linux/video0/device` → the
/// USB device node above it. Symmetric to `resolve_usb_topo` for netdevs.
#[cfg(target_os = "linux")]
pub async fn resolve_usb_topo_for_video(dev_path: &str) -> Option<UsbTopo> {
    let name = Path::new(dev_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| dev_path.to_string());
    let link = format!("/sys/class/video4linux/{}/device", name);
    let start = tokio::fs::canonicalize(&link).await.ok()?;
    topo_from_device_dir(&start)
}

/// Resolve a tty device's USB topology (the FC, e.g. `ttyACM0`), or `None` when
/// it is not USB-backed. Used to add the FC to the protected set.
#[cfg(target_os = "linux")]
pub async fn resolve_usb_topo_for_tty(tty: &str) -> Option<UsbTopo> {
    let link = format!("/sys/class/tty/{}/device", tty);
    let start = tokio::fs::canonicalize(&link).await.ok()?;
    topo_from_device_dir(&start)
}

/// True when a hub's downstream port exposes the per-port `disable` attribute,
/// so the camera's port can be cleanly re-enabled without touching its siblings.
/// The Pi 4B internal hub does NOT expose this; an external hub may.
#[cfg(target_os = "linux")]
pub fn ppps_capable(hub: &str, port: u32) -> bool {
    Path::new("/sys/bus/usb/devices")
        .join(port_disable_rel(hub, port))
        .is_file()
}

/// Walk a resolved `/sys` device path up to the USB device node holding the id
/// files, then collect its USB-node ancestors. Pure (sync fs reads on a real
/// path) so a fixture sysfs tree exercises it off a real SBC. `None` when the
/// netdev is not USB-backed.
#[cfg(any(target_os = "linux", test))]
pub fn topo_from_device_dir(start: &Path) -> Option<UsbTopo> {
    const MAX_HOPS: usize = 8;
    // Find the device node (the first ancestor holding idVendor + idProduct).
    let mut dir = start.to_path_buf();
    let mut device_dir = None;
    for _ in 0..=MAX_HOPS {
        if dir.join("idVendor").is_file() && dir.join("idProduct").is_file() {
            device_dir = Some(dir.clone());
            break;
        }
        match dir.parent() {
            Some(p) => dir = p.to_path_buf(),
            None => break,
        }
    }
    let device_dir = device_dir?;
    let bind_id = device_dir.file_name()?.to_string_lossy().to_string();
    // Collect USB-node ancestors above the device node, up to the root hub.
    let mut ancestors = Vec::new();
    let mut cur = device_dir.parent().map(|p| p.to_path_buf());
    for _ in 0..MAX_HOPS {
        let Some(c) = cur else {
            break;
        };
        let name = c
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if is_usb_node_name(&name) {
            ancestors.push(name);
            cur = c.parent().map(|p| p.to_path_buf());
        } else {
            break;
        }
    }
    Some(UsbTopo { bind_id, ancestors })
}

/// Resolve a netdev's USB topology, or `None` when it is not USB-backed.
#[cfg(target_os = "linux")]
pub async fn resolve_usb_topo(iface: &str) -> Option<UsbTopo> {
    let link = format!("/sys/class/net/{}/device", iface);
    let start = tokio::fs::canonicalize(&link).await.ok()?;
    topo_from_device_dir(&start)
}

/// Classify the control interface's USB relationship for the guard.
#[cfg(target_os = "linux")]
pub async fn resolve_control_path(default_iface: Option<&str>) -> ControlPath {
    let Some(iface) = default_iface else {
        return ControlPath::NoRoute;
    };
    match resolve_usb_topo(iface).await {
        Some(t) => ControlPath::Usb(t),
        // The control interface resolves to a non-USB device (or has no backing
        // device): a USB unbind cannot reach it.
        None => ControlPath::NonUsb,
    }
}

#[cfg(test)]
mod tests;
