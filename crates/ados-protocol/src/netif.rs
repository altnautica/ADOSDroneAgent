//! Which wireless interface is which.
//!
//! A ground station carries two radios: the onboard chip (Broadcom `brcmfmac`
//! on a Pi) and a USB RTL8812EU running WFB in monitor mode. Their kernel names
//! are NOT stable — measured on the bench, `wlan0` was the onboard chip on two
//! boots out of three and the flight radio on the third. Every consumer that
//! hardcoded `"wlan0"` was therefore correct only by luck, and on the unlucky
//! boot would configure an access point on the aircraft's radio link.
//!
//! The repo already understood this failure mode for ethernet: the uplink
//! router resolves its NIC lazily "because early boot can race with udev: the
//! NIC may not yet have its predictable name when this module is imported". The
//! same reasoning applies to WiFi, and this module supplies the answer.
//!
//! Classification reads the GENERATED tables in [`crate::wfb_tables`] rather
//! than a hand-written driver list. Three hand-written copies of that list
//! already exist in the tree, which is exactly how such lists drift apart.
//!
//! The pure half ([`choose_ap_interface`]) is separated from the sysfs half so
//! both name orderings can be tested without a radio present — the ordering IS
//! the bug, so it has to be exercised deliberately.

use std::path::{Path, PathBuf};

use crate::wfb_tables::{DENY_DRIVER_PREFIXES, DENY_VID, WFB_COMPATIBLE, WFB_COMPATIBLE_DRIVERS};

/// A wireless interface and the kernel driver bound to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WirelessIface {
    pub name: String,
    pub driver: String,
}

/// Why no access-point interface could be chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApIfaceError {
    /// The box has no onboard WiFi at all. Running the access point on the
    /// flight radio is never the better outcome, so the AP does not start.
    NoOnboardWifi,
    /// An operator pinned an interface that is the WFB radio.
    ConfiguredIsRadio { iface: String, driver: String },
    /// An operator pinned an interface that is not present.
    ConfiguredMissing { iface: String },
}

impl std::fmt::Display for ApIfaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoOnboardWifi => write!(
                f,
                "no onboard WiFi interface found; refusing to run the access \
                 point on the WFB radio"
            ),
            Self::ConfiguredIsRadio { iface, driver } => write!(
                f,
                "configured AP interface {iface} is the WFB radio (driver \
                 {driver}); refusing to take the aircraft's radio link"
            ),
            Self::ConfiguredMissing { iface } => {
                write!(f, "configured AP interface {iface} is not present")
            }
        }
    }
}

/// True when this driver is a WFB injection radio (the flight link).
///
/// Reads the generated compatible-driver table so it cannot drift from the
/// radio's own adapter selection.
pub fn is_injection_driver(driver: &str) -> bool {
    let d = driver.trim().to_ascii_lowercase();
    WFB_COMPATIBLE_DRIVERS.iter().any(|k| *k == d)
}

/// True when an adapter is a WFB injection radio by the rule the radio's own
/// adapter selection applies: the management-WiFi deny set first (driver
/// prefix or vendor id), then the USB VID:PID table, then the compatible-driver
/// table. The VID:PID leg is what catches a radio bound to a driver outside the
/// driver table (a mainline driver on a known flight-radio id).
pub fn is_wfb_adapter(driver: &str, vid_pid: Option<(u16, u16)>) -> bool {
    let d = driver.trim().to_ascii_lowercase();
    if DENY_DRIVER_PREFIXES.iter().any(|p| d.starts_with(p)) {
        return false;
    }
    if let Some((vid, pid)) = vid_pid {
        if DENY_VID.contains(&vid) {
            return false;
        }
        if WFB_COMPATIBLE
            .iter()
            .any(|(v, p, _)| *v == vid && *p == pid)
        {
            return true;
        }
    }
    is_injection_driver(&d)
}

/// `(idVendor, idProduct)` of the USB device backing a netdev under `root`.
///
/// The netdev's `device` link points at the USB *interface* node, which has no
/// id files; they live on the parent *device* node, one (sometimes more) levels
/// up. The walk is bounded so a non-USB device never climbs to the root.
pub fn usb_vid_pid_in(root: &Path, iface: &str) -> Option<(u16, u16)> {
    const MAX_PARENT_HOPS: usize = 4;
    let mut dir = std::fs::canonicalize(root.join(iface).join("device")).ok()?;
    for _ in 0..=MAX_PARENT_HOPS {
        if let (Ok(vid), Ok(pid)) = (
            std::fs::read_to_string(dir.join("idVendor")),
            std::fs::read_to_string(dir.join("idProduct")),
        ) {
            return Some((
                u16::from_str_radix(vid.trim(), 16).ok()?,
                u16::from_str_radix(pid.trim(), 16).ok()?,
            ));
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

/// [`usb_vid_pid_in`] against the live `/sys/class/net`.
pub fn usb_vid_pid(iface: &str) -> Option<(u16, u16)> {
    usb_vid_pid_in(Path::new(NET_DIR), iface)
}

/// True when this driver is onboard management WiFi — the access-point radio.
///
/// This is the generated deny-set the radio uses to make sure it never grabs
/// management WiFi for injection. Read from the other direction it is exactly
/// "the interface the access point wants", which is why it is reused here
/// rather than a second list being invented.
pub fn is_onboard_wifi_driver(driver: &str) -> bool {
    let d = driver.trim().to_ascii_lowercase();
    DENY_DRIVER_PREFIXES.iter().any(|p| d.starts_with(p))
}

/// Choose the access-point interface. Pure — no filesystem, no radio.
///
/// Order: an explicit operator setting, then the first onboard-WiFi driver.
/// `radio_iface` is the interface the radio reports it actually took (from its
/// own sidecar); anything matching it is refused outright, so even a wrong
/// classification cannot end with hostapd on the flight link.
pub fn choose_ap_interface(
    ifaces: &[WirelessIface],
    configured: &str,
    radio_iface: Option<&str>,
) -> Result<String, ApIfaceError> {
    let is_radio = |c: &WirelessIface| {
        is_injection_driver(&c.driver) || radio_iface.is_some_and(|r| r == c.name)
    };

    let configured = configured.trim();
    if !configured.is_empty() {
        return match ifaces.iter().find(|c| c.name == configured) {
            Some(c) if is_radio(c) => Err(ApIfaceError::ConfiguredIsRadio {
                iface: c.name.clone(),
                driver: c.driver.clone(),
            }),
            Some(c) => Ok(c.name.clone()),
            // Absent is an error rather than a silent fallback: an operator who
            // named an interface should be told it is missing, not quietly
            // given a different one.
            None => Err(ApIfaceError::ConfiguredMissing {
                iface: configured.to_string(),
            }),
        };
    }

    ifaces
        .iter()
        .find(|c| is_onboard_wifi_driver(&c.driver) && !is_radio(c))
        .map(|c| c.name.clone())
        .ok_or(ApIfaceError::NoOnboardWifi)
}

const NET_DIR: &str = "/sys/class/net";

/// The kernel driver bound to an interface, from the sysfs `device/driver` link.
pub fn driver_name_in(root: &Path, iface: &str) -> Option<String> {
    let link = root.join(iface).join("device").join("driver");
    let target = std::fs::read_link(&link).ok()?;
    Some(target.file_name()?.to_string_lossy().into_owned())
}

/// True when the interface is 802.11 (it carries a `phy80211` node).
pub fn is_wireless_in(root: &Path, iface: &str) -> bool {
    root.join(iface).join("phy80211").exists() || root.join(iface).join("wireless").exists()
}

/// Every wireless interface with its driver, sorted by name for determinism.
///
/// Sorted so the choice does not depend on readdir order, which is arbitrary.
/// Determinism matters more than the specific order: an interface with no
/// resolvable driver is dropped rather than guessed at.
pub fn list_wireless_in(root: &Path) -> Vec<WirelessIface> {
    let mut out: Vec<WirelessIface> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name == "lo" || !is_wireless_in(root, &name) {
                return None;
            }
            Some(WirelessIface {
                driver: driver_name_in(root, &name)?,
                name,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// [`list_wireless_in`] against the live `/sys/class/net`.
pub fn list_wireless() -> Vec<WirelessIface> {
    list_wireless_in(&PathBuf::from(NET_DIR))
}

/// The `wfb-stats` sidecar (contract id `wfb-stats`), where the radio publishes
/// the interface it actually selected.
pub const WFB_STATS_PATH: &str = "/run/ados/wfb-stats.json";

/// The interface the radio reports it took, if it has published one.
///
/// Consuming the radio's own verdict rather than re-deriving it is the pattern
/// the USB-rehome guard already uses to build its protected set. It is the
/// authority here: classification can be wrong, but the radio knows what it
/// opened.
pub fn radio_interface_from(path: &Path) -> Option<String> {
    let txt = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
    let iface = v.get("interface")?.as_str()?.trim().to_string();
    (!iface.is_empty()).then_some(iface)
}

/// [`radio_interface_from`] against the live sidecar.
pub fn radio_interface() -> Option<String> {
    radio_interface_from(Path::new(WFB_STATS_PATH))
}

/// Resolve the access-point interface on this box.
pub fn resolve_ap_interface(
    configured: &str,
    radio_iface: Option<&str>,
) -> Result<String, ApIfaceError> {
    choose_ap_interface(&list_wireless(), configured, radio_iface)
}

/// Drivers that present an Ethernet-class interface without being a wired
/// port: the USB gadget the board itself exposes to a laptop, a phone's USB
/// tether, and cellular modems. Their interfaces are uplinks or lifelines of
/// their own, never the operator's cable.
const NOT_WIRED_DRIVERS: &[&str] = &[
    "g_ether",
    "g_multi",
    "rndis_host",
    "ipheth",
    "qmi_wwan",
    "cdc_mbim",
    "huawei_cdc_ncm",
];

/// True when the interface is a physical wired Ethernet port: Ethernet link
/// type, backed by a device with a bound driver, not 802.11, and not a USB
/// gadget, phone tether or modem. Classified by device, never by name, since
/// wired names vary across boards (`eth0`, `end1`, `enp1s0`, `enx…`).
pub fn is_wired_in(root: &Path, iface: &str) -> bool {
    if iface == "lo" || is_wireless_in(root, iface) {
        return false;
    }
    let dir = root.join(iface);
    // ARPHRD_ETHER.
    let ether = std::fs::read_to_string(dir.join("type"))
        .map(|t| t.trim() == "1")
        .unwrap_or(false);
    if !ether {
        return false;
    }
    // A gadget function's interface hangs off the board's own UDC.
    let device = match std::fs::read_link(dir.join("device")) {
        Ok(target) => target,
        Err(_) => return false,
    };
    if device.to_string_lossy().contains("gadget") {
        return false;
    }
    match driver_name_in(root, iface) {
        Some(driver) => {
            !NOT_WIRED_DRIVERS.contains(&driver.as_str()) && !driver.starts_with("usb_f_")
        }
        None => false,
    }
}

/// Every physical wired interface, sorted by name for determinism.
pub fn list_wired_in(root: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| is_wired_in(root, name))
        .collect();
    out.sort();
    out
}

/// [`list_wired_in`] against the live `/sys/class/net`.
pub fn list_wired() -> Vec<String> {
    list_wired_in(&PathBuf::from(NET_DIR))
}

/// True when the box carries two or more WFB-class radios, the hardware a
/// relay or receiver mesh role needs. Derived live from driver and USB ids so
/// it follows a replug, never from a persisted hint.
pub fn mesh_capable_in(root: &Path) -> bool {
    list_wireless_in(root)
        .iter()
        .filter(|w| is_wfb_adapter(&w.driver, usb_vid_pid_in(root, &w.name)))
        .count()
        >= 2
}

/// [`mesh_capable_in`] against the live `/sys/class/net`.
pub fn mesh_capable() -> bool {
    mesh_capable_in(Path::new(NET_DIR))
}

/// Drivers that only ever bind a cellular modem's data interface.
const CELLULAR_DRIVERS: &[&str] = &[
    "qmi_wwan",
    "cdc_mbim",
    "huawei_cdc_ncm",
    "cdc_ncm",
    "rndis_host",
];

/// USB vendors whose `option`-driven interfaces are modems.
const MODEM_USB_VIDS: &[u16] = &[0x1199, 0x2c7c, 0x12d1, 0x05c6, 0x1e0e, 0x2cb7, 0x1bc7];

/// True when the interface is a cellular modem's data port, by driver and
/// bus. The board's own USB gadget (a `gadget`/`udc` device path) is never a
/// modem even when it binds the same class driver.
pub fn is_cellular_in(root: &Path, iface: &str) -> bool {
    let Ok(device) = std::fs::read_link(root.join(iface).join("device")) else {
        return false;
    };
    let path = device.to_string_lossy();
    if path.contains("gadget") || path.contains("udc") {
        return false;
    }
    match driver_name_in(root, iface).as_deref() {
        Some(d) if CELLULAR_DRIVERS.contains(&d) => true,
        Some("option") => {
            usb_vid_pid_in(root, iface).is_some_and(|(vid, _)| MODEM_USB_VIDS.contains(&vid))
        }
        _ => false,
    }
}

/// Every cellular data interface, sorted by name for determinism.
pub fn list_cellular_in(root: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| is_cellular_in(root, name))
        .collect();
    out.sort();
    out
}

/// [`list_cellular_in`] against the live `/sys/class/net`.
pub fn list_cellular() -> Vec<String> {
    list_cellular_in(&PathBuf::from(NET_DIR))
}

#[cfg(test)]
#[path = "netif_tests.rs"]
mod tests;
