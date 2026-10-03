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

#[cfg(test)]
mod tests {
    use super::*;

    fn ifaces(pairs: &[(&str, &str)]) -> Vec<WirelessIface> {
        pairs
            .iter()
            .map(|(n, d)| WirelessIface {
                name: (*n).to_string(),
                driver: (*d).to_string(),
            })
            .collect()
    }

    #[test]
    fn drivers_classify_from_the_generated_tables() {
        assert!(is_injection_driver("rtl88x2eu"));
        assert!(
            is_injection_driver("RTL8812EU"),
            "compare is case-insensitive"
        );
        assert!(!is_injection_driver("brcmfmac"));

        assert!(is_onboard_wifi_driver("brcmfmac"));
        assert!(
            is_onboard_wifi_driver("brcmfmac_sdio"),
            "the deny-set is prefix-matched"
        );
        assert!(is_onboard_wifi_driver("aic8800"));
        assert!(!is_onboard_wifi_driver("rtl88x2eu"));
    }

    #[test]
    fn a_known_radio_id_is_a_wfb_adapter_whatever_driver_it_bound() {
        let (vid, pid, _) = WFB_COMPATIBLE[0];
        // A mainline driver outside the driver table, on a flight-radio id.
        assert!(is_wfb_adapter("rtw88_8822bu", Some((vid, pid))));
        // The driver table still answers when the ids could not be read.
        assert!(is_wfb_adapter("rtl88x2eu", None));
        // Management WiFi is denied before either table is consulted.
        assert!(!is_wfb_adapter("brcmfmac", Some((vid, pid))));
        assert!(!is_wfb_adapter("rtl88x2eu", Some((DENY_VID[0], 0x0001))));
        assert!(!is_wfb_adapter("rtw88_8822bu", Some((0x1234, 0x5678))));
    }

    #[test]
    fn usb_ids_are_read_from_the_parent_device_node() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("usb1").join("1-1");
        let interface = device.join("1-1:1.0");
        std::fs::create_dir_all(&interface).unwrap();
        std::fs::write(device.join("idVendor"), "0bda\n").unwrap();
        std::fs::write(device.join("idProduct"), "a81a\n").unwrap();
        let net = dir.path().join("net");
        std::fs::create_dir_all(net.join("wlan1")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&interface, net.join("wlan1").join("device")).unwrap();
        assert_eq!(usb_vid_pid_in(&net, "wlan1"), Some((0x0BDA, 0xA81A)));
        assert_eq!(usb_vid_pid_in(&net, "eth0"), None);
    }

    /// The bug itself: the same two devices under both name orderings must
    /// yield the same answer. Measured on the bench across three reboots.
    #[test]
    fn the_choice_survives_the_names_racing() {
        let onboard_first = ifaces(&[("wlan0", "brcmfmac"), ("wlan1", "rtl88x2eu")]);
        let radio_first = ifaces(&[("wlan0", "rtl88x2eu"), ("wlan1", "brcmfmac")]);

        assert_eq!(
            choose_ap_interface(&onboard_first, "", None).unwrap(),
            "wlan0"
        );
        assert_eq!(
            choose_ap_interface(&radio_first, "", None).unwrap(),
            "wlan1",
            "when the radio takes the wlan0 name the AP must follow the onboard chip"
        );
    }

    #[test]
    fn the_radios_own_verdict_overrides_classification() {
        // Even if the driver were misclassified, the interface the radio says
        // it took is refused. Belt and braces, because the cost of being wrong
        // is taking down the aircraft's link.
        let list = ifaces(&[("wlan0", "brcmfmac"), ("wlan1", "brcmfmac")]);
        assert_eq!(
            choose_ap_interface(&list, "", Some("wlan0")).unwrap(),
            "wlan1"
        );
    }

    #[test]
    fn a_box_with_only_the_radio_gets_no_access_point() {
        let list = ifaces(&[("wlan0", "rtl88x2eu")]);
        assert_eq!(
            choose_ap_interface(&list, "", None),
            Err(ApIfaceError::NoOnboardWifi)
        );
    }

    #[test]
    fn an_operator_pin_is_honoured_but_never_onto_the_radio() {
        let list = ifaces(&[("wlan0", "brcmfmac"), ("wlan1", "rtl88x2eu")]);
        assert_eq!(choose_ap_interface(&list, "wlan0", None).unwrap(), "wlan0");
        assert!(matches!(
            choose_ap_interface(&list, "wlan1", None),
            Err(ApIfaceError::ConfiguredIsRadio { .. })
        ));
        assert!(matches!(
            choose_ap_interface(&list, "wlan9", None),
            Err(ApIfaceError::ConfiguredMissing { .. })
        ));
    }

    #[test]
    fn the_radio_sidecar_is_read_and_blanks_are_not_a_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("wfb-stats.json");

        std::fs::write(&p, r#"{"interface":"wlan1","rssi":-36}"#).unwrap();
        assert_eq!(radio_interface_from(&p).as_deref(), Some("wlan1"));

        // A blank or absent field is "the radio has not said", NOT "no radio" —
        // it must not read as a verdict that some interface is free.
        std::fs::write(&p, r#"{"interface":"  "}"#).unwrap();
        assert_eq!(radio_interface_from(&p), None);
        std::fs::write(&p, r#"{"rssi":-36}"#).unwrap();
        assert_eq!(radio_interface_from(&p), None);
        assert_eq!(radio_interface_from(Path::new("/nope/absent.json")), None);
    }

    #[test]
    fn enumeration_is_sorted_and_skips_what_it_cannot_identify() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Two wireless ifaces created out of alphabetical order, plus one with
        // no driver link at all, plus a non-wireless one.
        for (name, driver) in [("wlan1", Some("rtl88x2eu")), ("wlan0", Some("brcmfmac"))] {
            let d = root.join(name);
            std::fs::create_dir_all(d.join("phy80211")).unwrap();
            if let Some(drv) = driver {
                let target = root.join("drivers").join(drv);
                std::fs::create_dir_all(&target).unwrap();
                std::fs::create_dir_all(d.join("device")).unwrap();
                std::os::unix::fs::symlink(&target, d.join("device").join("driver")).unwrap();
            }
        }
        std::fs::create_dir_all(root.join("wlan7").join("phy80211")).unwrap(); // no driver
        std::fs::create_dir_all(root.join("eth0")).unwrap(); // not wireless

        let found = list_wireless_in(root);
        assert_eq!(
            found,
            ifaces(&[("wlan0", "brcmfmac"), ("wlan1", "rtl88x2eu")]),
            "sorted, wireless only, and an unidentifiable iface is dropped"
        );
    }

    #[test]
    fn wired_ports_are_classified_by_device_not_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // (iface, device path fragment, driver, link type)
        let rigs = [
            ("end1", "platform/fe300000.ethernet", "rk_gmac-dwmac", "1"),
            ("enx001122334455", "usb1/1-1/1-1:1.0", "r8152", "1"),
            ("usb0", "platform/fe980000.usb/gadget", "g_ether", "1"),
            ("eth1", "usb1/1-2/1-2:1.0", "rndis_host", "1"),
            ("wwan0", "usb1/1-3/1-3:1.4", "qmi_wwan", "65534"),
        ];
        for (name, device, driver, link_type) in rigs {
            let dev = root.join("devices").join(device);
            let drv = root.join("drivers").join(driver);
            std::fs::create_dir_all(&dev).unwrap();
            std::fs::create_dir_all(&drv).unwrap();
            std::os::unix::fs::symlink(&drv, dev.join("driver")).unwrap();
            let iface = root.join(name);
            std::fs::create_dir_all(&iface).unwrap();
            std::fs::write(iface.join("type"), format!("{link_type}\n")).unwrap();
            std::os::unix::fs::symlink(&dev, iface.join("device")).unwrap();
        }
        // A virtual bridge: Ethernet type, no backing device.
        std::fs::create_dir_all(root.join("br0")).unwrap();
        std::fs::write(root.join("br0").join("type"), "1\n").unwrap();

        assert_eq!(
            list_wired_in(root),
            vec!["end1".to_string(), "enx001122334455".to_string()],
            "gadget, tether, modem and virtual interfaces are not wired ports"
        );
    }
}
