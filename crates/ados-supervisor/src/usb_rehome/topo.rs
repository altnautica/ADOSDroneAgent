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
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn usb_node_name_classification() {
        assert!(is_usb_node_name("1-1"));
        assert!(is_usb_node_name("2-1.4.3"));
        assert!(is_usb_node_name("usb1"));
        assert!(is_usb_node_name("usb2"));
        // The interface node (colon) and the controller (letters) are not nodes.
        assert!(!is_usb_node_name("1-1:1.0"));
        assert!(!is_usb_node_name("xhci-hcd.0.auto"));
        assert!(!is_usb_node_name("0000:00:14.0"));
        assert!(!is_usb_node_name("platform"));
        assert!(!is_usb_node_name(""));
    }

    fn topo(bind: &str, ancestors: &[&str]) -> UsbTopo {
        UsbTopo {
            bind_id: bind.to_string(),
            ancestors: ancestors.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn guard_no_route_is_fail_closed() {
        let t = topo("1-1", &["usb1"]);
        assert_eq!(
            guard_verdict(&t, &ControlPath::NoRoute),
            GuardVerdict::BlockUnprovable
        );
    }

    #[test]
    fn guard_non_usb_control_allows() {
        let t = topo("1-1", &["usb1"]);
        assert_eq!(guard_verdict(&t, &ControlPath::NonUsb), GuardVerdict::Allow);
    }

    #[test]
    fn guard_blocks_same_device() {
        let t = topo("1-1", &["usb1"]);
        let c = ControlPath::Usb(topo("1-1", &["usb1"]));
        assert_eq!(guard_verdict(&t, &c), GuardVerdict::BlockIsControl);
    }

    #[test]
    fn guard_blocks_control_under_target_hub() {
        // Target is the hub "1-1"; the control device "1-1.4" hangs off it.
        let target = topo("1-1", &["usb1"]);
        let control = ControlPath::Usb(topo("1-1.4", &["1-1", "usb1"]));
        assert_eq!(
            guard_verdict(&target, &control),
            GuardVerdict::BlockSharesBranch
        );
    }

    #[test]
    fn guard_allows_disjoint_usb_devices() {
        // The WFB adapter on 1-1 and the control link on 1-2 share only the root
        // hub; a soft unbind of 1-1 cannot touch 1-2 → allowed.
        let target = topo("1-1", &["usb1"]);
        let control = ControlPath::Usb(topo("1-2", &["usb1"]));
        assert_eq!(guard_verdict(&target, &control), GuardVerdict::Allow);
        // Control on a different controller is also fine.
        let control2 = ControlPath::Usb(topo("2-1", &["usb2"]));
        assert_eq!(guard_verdict(&target, &control2), GuardVerdict::Allow);
    }

    #[test]
    fn topo_walk_finds_bind_id_and_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        // .../usb1/1-1/{idVendor,idProduct} and the interface node 1-1/1-1:1.0
        let device = dir.path().join("usb1").join("1-1");
        let iface_node = device.join("1-1:1.0");
        fs::create_dir_all(&iface_node).unwrap();
        fs::write(device.join("idVendor"), "0bda\n").unwrap();
        fs::write(device.join("idProduct"), "a81a\n").unwrap();

        let t = topo_from_device_dir(&iface_node).unwrap();
        assert_eq!(t.bind_id, "1-1");
        assert_eq!(t.ancestors, vec!["usb1".to_string()]);
    }

    #[test]
    fn topo_walk_none_when_not_usb() {
        let dir = tempfile::tempdir().unwrap();
        // A device dir with no idVendor/idProduct anywhere in the bounded walk.
        let node = dir.path().join("platform").join("eth0-node");
        fs::create_dir_all(&node).unwrap();
        assert!(topo_from_device_dir(&node).is_none());
    }
}
