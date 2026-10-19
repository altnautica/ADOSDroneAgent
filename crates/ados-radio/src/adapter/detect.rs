//! WiFi adapter discovery and WFB-ng classification.
//!
//! Scans `/sys/class/net` and classifies each radio against the canonical WFB
//! compatibility table + driver-name fallback (denying management WiFi first).
//! The pure classifier (`build_adapter_info`) and the sysfs parent-walks are
//! unit-testable off a fixture tree without touching a real SBC. Ranking and
//! monitor-mode verification of the candidates live in `select`.

use serde::Serialize;

// The WFB adapter classification table, the driver-name fallback, and the
// management-WiFi deny-sets are the generated `ados_protocol::wfb_tables`
// const, the single source of truth shared with the Python adapter (whose
// generated copy is `services/wfb/_wfb_tables_generated.py`). The source file
// is `crates/ados-protocol/wfb-adapters.toml`; regenerate with
// `cargo run -p ados-capabilities-codegen`. The local aliases below keep the
// rest of this module reading against the short names.
#[cfg(any(target_os = "linux", test))]
use ados_protocol::wfb_tables::{
    DENY_DRIVER_PREFIXES, DENY_VID, WFB_COMPATIBLE, WFB_COMPATIBLE_DRIVERS,
};

/// An RTL adapter needs high-speed (480 Mbps) USB to push WFB RF. A reading
/// below that (12 Mbps full-speed) means the device fell back to a companion
/// controller / a flaky port and cannot reliably radiate. `None` (not USB /
/// unreadable) is treated as not-degraded so a non-USB or unknown adapter is
/// never falsely flagged.
pub fn usb_speed_degraded(speed_mbps: Option<u32>) -> bool {
    matches!(speed_mbps, Some(s) if s < 480)
}

/// Full per-adapter detection record. This is the one source of truth the
/// permanent-Python seam (REST + bind iface setup) and pre-service callers read
/// off `/run/ados/wfb-adapters.json` and the one-shot `adapters` CLI mode. Field
/// names match the Python `WifiAdapterInfo` dataclass so the JSON is identical
/// whether the Rust service or the Python module produced it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WifiAdapterInfo {
    pub interface_name: String,
    pub driver: String,
    pub chipset: String,
    pub supports_monitor: bool,
    /// The interface's current operating mode ("monitor" | "managed" | …), or
    /// `None` when it could not be read (serialized as JSON null).
    pub current_mode: Option<String>,
    pub phy: String,
    /// USB vendor id, or `None` when the iface is not USB-backed.
    pub usb_vid: Option<u16>,
    /// USB product id, or `None` when the iface is not USB-backed.
    pub usb_pid: Option<u16>,
    /// Enumerated USB link speed in Mbps (12 full-speed, 480 high-speed, 5000
    /// SuperSpeed), or `None` when not USB-backed / unreadable. An RTL8812EU that
    /// enumerates at 12 (a flaky port / companion-controller fallback) cannot
    /// push real RF even though the driver loads and the tx_bytes counter moves.
    pub usb_speed_mbps: Option<u32>,
    pub is_wfb_compatible: bool,
    /// Supported interface modes advertised by the wiphy (e.g. monitor, managed).
    pub capabilities: Vec<String>,
}

/// Detect every WiFi adapter and classify each for WFB-ng injection. This is
/// the public adapter contract: it returns the FULL list (compatible and not),
/// never raises, and is what the one-shot CLI mode prints and what the service
/// writes to `/run/ados/wfb-adapters.json`. The operator's control-path iface
/// (default route) is excluded so monitor mode never severs the management link.
///
/// On non-Linux hosts this returns an empty list.
#[cfg(target_os = "linux")]
pub async fn detect_wfb_adapters() -> Vec<WifiAdapterInfo> {
    let mut out = Vec::new();
    let Ok(mut ifaces) = tokio::fs::read_dir("/sys/class/net").await else {
        return out;
    };
    let control_iface = super::control_interface().await;
    while let Ok(Some(entry)) = ifaces.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if !is_wireless(&name).await {
            continue;
        }
        if control_iface.as_deref() == Some(name.as_str()) {
            tracing::info!(interface = %name, "wfb_adapter_excluded_control_iface");
            continue;
        }
        let driver = driver_name(&name).await;
        let driver = if driver.is_empty() {
            "unknown".to_string()
        } else {
            driver
        };
        let vid_pid = usb_vid_pid(&name).await;
        let phy = phy_for_iface(&name).await.unwrap_or_default();
        let capabilities = supported_modes(&phy).await;
        let supports_monitor = capabilities.iter().any(|m| m == "monitor");
        let current_mode = super::monitor::get_interface_mode(&name).await;
        let usb_speed = usb_speed_mbps(&name).await;
        let mut info = build_adapter_info(
            name,
            driver,
            vid_pid,
            phy,
            capabilities,
            supports_monitor,
            current_mode,
        );
        info.usb_speed_mbps = usb_speed;
        out.push(info);
    }
    let compatible = out.iter().filter(|a| a.is_wfb_compatible).count();
    let monitor_capable = out.iter().filter(|a| a.supports_monitor).count();
    tracing::info!(
        total = out.len(),
        compatible,
        monitor_capable,
        "wfb_adapter_scan"
    );
    out
}

#[cfg(not(target_os = "linux"))]
pub async fn detect_wfb_adapters() -> Vec<WifiAdapterInfo> {
    Vec::new()
}

/// Build a `WifiAdapterInfo` from the raw probe facts, applying the deny gate
/// (management WiFi can never be flagged compatible) then the VID:PID table and
/// the driver-name fallback. Pure so the classification is unit-testable without
/// touching sysfs. Mirrors the per-iface body of Python `detect_wfb_adapters`.
#[cfg(any(target_os = "linux", test))]
pub(super) fn build_adapter_info(
    interface_name: String,
    driver: String,
    vid_pid: Option<(u16, u16)>,
    phy: String,
    capabilities: Vec<String>,
    supports_monitor: bool,
    current_mode: Option<String>,
) -> WifiAdapterInfo {
    let (usb_vid, usb_pid) = match vid_pid {
        Some((v, p)) => (Some(v), Some(p)),
        None => (None, None),
    };

    // Hard deny known management-WiFi radios before any compat path runs.
    if is_denied(&driver, vid_pid) {
        let chipset = if driver.is_empty() {
            "management-wifi".to_string()
        } else {
            driver.clone()
        };
        return WifiAdapterInfo {
            interface_name,
            driver,
            chipset,
            supports_monitor,
            current_mode,
            phy,
            usb_vid,
            usb_pid,
            usb_speed_mbps: None,
            is_wfb_compatible: false,
            capabilities,
        };
    }

    let mut chipset = driver.clone();
    let mut is_compat = false;
    if let Some((vid, pid)) = vid_pid {
        if let Some(label) = wfb_compatible_label(vid, pid) {
            chipset = label.to_string();
            // Disambiguate the (0BDA:A81A) PID: the bound EU kernel driver is
            // the authoritative signal that this is the EU silicon.
            if (vid, pid) == (0x0BDA, 0xA81A) && driver.eq_ignore_ascii_case("rtl88x2eu") {
                chipset = "RTL8812EU (a81a)".to_string();
            }
            is_compat = true;
        }
    }
    if !is_compat && driver_is_wfb_compatible(&driver) {
        is_compat = true;
        if chipset.is_empty() || chipset == driver {
            chipset = driver.clone();
        }
    }

    WifiAdapterInfo {
        interface_name,
        driver,
        chipset,
        supports_monitor,
        current_mode,
        phy,
        usb_vid,
        usb_pid,
        // The link speed is filled in by the detect loop (it needs an async
        // sysfs read); the pure classifier leaves it None.
        usb_speed_mbps: None,
        is_wfb_compatible: is_compat,
        capabilities,
    }
}

#[cfg(any(target_os = "linux", test))]
fn is_denied(driver: &str, vid_pid: Option<(u16, u16)>) -> bool {
    if DENY_DRIVER_PREFIXES.iter().any(|p| driver.starts_with(p)) {
        return true;
    }
    if let Some((vid, _pid)) = vid_pid {
        if DENY_VID.contains(&vid) {
            return true;
        }
    }
    false
}

/// Classify a (driver, VID:PID) into a chipset label + injection rank. Retained
/// as the unit-test entry point for the driver-precedence + table-lookup logic
/// that `build_adapter_info` exercises in the live path.
#[cfg(test)]
fn classify(driver: &str, vid_pid: Option<(u16, u16)>) -> (String, u8) {
    // Driver-name takes precedence for PID-ambiguous adapters.
    if driver.contains("rtl88x2eu") || driver.contains("8812eu") {
        return ("RTL8812EU".to_string(), 0);
    }
    if driver.contains("rtl8812au") || driver.contains("88xxau") {
        return ("RTL8812AU".to_string(), 1);
    }
    if let Some((vid, pid)) = vid_pid {
        if let Some(label) = wfb_compatible_label(vid, pid) {
            let rank = if label.contains("EU") { 0 } else { 1 };
            return (label.to_string(), rank);
        }
    }
    (format!("unknown (driver={})", driver), 3)
}

/// Return the chipset label for a (VID, PID) in the known WFB-compatible table,
/// or `None`. Single lookup against the canonical table.
#[cfg(any(target_os = "linux", test))]
pub(super) fn wfb_compatible_label(vid: u16, pid: u16) -> Option<&'static str> {
    WFB_COMPATIBLE
        .iter()
        .find(|(v, p, _)| *v == vid && *p == pid)
        .map(|(_, _, label)| *label)
}

/// True when a driver name (lower-cased) is one of the known WFB-ng DKMS
/// modules. Authoritative even when the USB ID walk missed the IDs (e.g. a USB
/// hub layer hides the parent device).
#[cfg(any(target_os = "linux", test))]
pub(super) fn driver_is_wfb_compatible(driver: &str) -> bool {
    let d = driver.trim().to_ascii_lowercase();
    WFB_COMPATIBLE_DRIVERS.contains(&d.as_str())
}

#[cfg(target_os = "linux")]
async fn is_wireless(iface: &str) -> bool {
    tokio::fs::metadata(format!("/sys/class/net/{}/wireless", iface))
        .await
        .is_ok()
        || tokio::fs::metadata(format!("/sys/class/net/{}/phy80211", iface))
            .await
            .is_ok()
}

#[cfg(target_os = "linux")]
async fn driver_name(iface: &str) -> String {
    // /sys/class/net/<if>/device/driver -> symlink ending with driver name.
    let link = format!("/sys/class/net/{}/device/driver", iface);
    tokio::fs::read_link(&link)
        .await
        .ok()
        .and_then(|p| p.file_name().map(|f| f.to_string_lossy().to_string()))
        .unwrap_or_default()
}

/// Read the wiphy name (e.g. `"phy0"`) bound to an interface from
/// `/sys/class/net/<if>/phy80211/name`, or `None` when absent.
#[cfg(target_os = "linux")]
async fn phy_for_iface(iface: &str) -> Option<String> {
    let path = format!("/sys/class/net/{}/phy80211/name", iface);
    tokio::fs::read_to_string(&path)
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The supported interface modes (e.g. `monitor`, `managed`) a wiphy advertises,
/// parsed out of `iw phy <phy> info`. Empty when the phy is unknown or `iw`
/// cannot be run.
#[cfg(target_os = "linux")]
async fn supported_modes(phy: &str) -> Vec<String> {
    if phy.is_empty() {
        return Vec::new();
    }
    match super::run_cmd_output("iw", &["phy", phy, "info"]).await {
        Ok(out) => parse_supported_modes(&out),
        Err(()) => Vec::new(),
    }
}

/// Read `(idVendor, idProduct)` for the USB device backing a netdev.
///
/// `/sys/class/net/<if>/device` is a symlink to the USB *interface* node
/// (e.g. `…/1-1:1.0`), which carries `bInterfaceClass` but NOT `idVendor` /
/// `idProduct` — those live on the parent USB *device* node (`…/1-1`). The
/// old read of `device/idVendor` therefore always missed on a USB adapter and
/// returned `None`, so the VID:PID table never disambiguated the (0BDA:A81A)
/// silicon and the driver-name fallback carried the whole classification.
///
/// Resolve the symlink to a real path, then walk UP at most a few parent dirs
/// until one holds both id files, and read them there. Bounded so a malformed
/// sysfs (or a non-USB iface) can never loop; `None` when not found.
#[cfg(target_os = "linux")]
async fn usb_vid_pid(iface: &str) -> Option<(u16, u16)> {
    // Resolve the `device` symlink to the USB interface node's real path, then
    // walk up to the parent USB device node holding the id files.
    let link = format!("/sys/class/net/{}/device", iface);
    let start = tokio::fs::canonicalize(&link).await.ok()?;
    vid_pid_from_device_dir(&start)
}

/// Read the enumerated USB link speed (Mbps) for the USB device backing a
/// netdev. The `speed` file lives on the same USB *device* node as `idVendor`
/// (e.g. `…/1-1/speed` → "480"), one or more parents above the interface node
/// the `device` symlink points at — so this mirrors [`usb_vid_pid`]'s
/// parent-walk. `None` when the iface is not USB-backed or the file is
/// unreadable. A value of 12 (full-speed) on an RTL8812EU is the degraded path.
#[cfg(target_os = "linux")]
async fn usb_speed_mbps(iface: &str) -> Option<u32> {
    let link = format!("/sys/class/net/{}/device", iface);
    let start = tokio::fs::canonicalize(&link).await.ok()?;
    speed_from_device_dir(&start)
}

/// Walk UP from a resolved USB interface node to the parent USB *device* node
/// carrying `speed`, and parse it as Mbps. Bounded to a few hops like
/// [`vid_pid_from_device_dir`]; `None` when no ancestor within the bound holds
/// the file. Pure so a fixture sysfs tree exercises it off a real SBC.
#[cfg(any(target_os = "linux", test))]
fn speed_from_device_dir(start: &std::path::Path) -> Option<u32> {
    const MAX_PARENT_HOPS: usize = 4;
    let mut dir = start.to_path_buf();
    for _ in 0..=MAX_PARENT_HOPS {
        if let Ok(raw) = std::fs::read_to_string(dir.join("speed")) {
            if let Ok(mbps) = raw.trim().parse::<u32>() {
                return Some(mbps);
            }
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => break,
        }
    }
    None
}

/// Walk UP from a resolved USB interface node to the parent USB *device* node
/// that carries `idVendor` + `idProduct`, and read them there. The netdev's
/// `device` symlink points at the interface node (e.g. `…:1.0`), which has no
/// id files; the ids live one (sometimes more) levels up. Bounded to a few
/// hops so a malformed tree (or a non-USB iface whose `device` resolves
/// elsewhere) can never loop or climb out to the controller root. `None` when
/// no ancestor within the bound holds both files. Pure (sync fs reads on a real
/// path) so a fixture sysfs tree exercises the parent-walk off a real SBC.
#[cfg(any(target_os = "linux", test))]
fn vid_pid_from_device_dir(start: &std::path::Path) -> Option<(u16, u16)> {
    // 4 hops covers interface → device (typically 1 hop, extra slack for hubs)
    // without reaching the controller root.
    const MAX_PARENT_HOPS: usize = 4;
    let mut dir = start.to_path_buf();
    for _ in 0..=MAX_PARENT_HOPS {
        if let (Ok(vid_raw), Ok(pid_raw)) = (
            std::fs::read_to_string(dir.join("idVendor")),
            std::fs::read_to_string(dir.join("idProduct")),
        ) {
            let vid = u16::from_str_radix(vid_raw.trim(), 16).ok()?;
            let pid = u16::from_str_radix(pid_raw.trim(), 16).ok()?;
            return Some((vid, pid));
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => break,
        }
    }
    None
}

/// Read an interface's MAC address from `/sys/class/net/<if>/address`,
/// lower-cased, or `None` when unreadable or empty.
#[cfg(target_os = "linux")]
pub(super) async fn mac_address(iface: &str) -> Option<String> {
    let path = format!("/sys/class/net/{}/address", iface);
    tokio::fs::read_to_string(&path)
        .await
        .ok()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
}

/// The USB device path (e.g. `1-1.2`) backing a netdev: the name of the USB
/// device node the `device` symlink's parent-walk lands on. Stable across
/// reboots for a given port, unlike the interface name. `None` when the iface
/// is not USB-backed.
#[cfg(target_os = "linux")]
pub(super) async fn usb_path(iface: &str) -> Option<String> {
    let link = format!("/sys/class/net/{}/device", iface);
    let start = tokio::fs::canonicalize(&link).await.ok()?;
    usb_path_from_device_dir(&start)
}

/// Walk UP from a resolved USB interface node to the USB *device* node that
/// carries `idVendor`, and return that node's directory name (the bus-port
/// path, e.g. `1-1.2`). Bounded like [`vid_pid_from_device_dir`]; `None` when
/// no ancestor within the bound is a USB device node.
#[cfg(any(target_os = "linux", test))]
fn usb_path_from_device_dir(start: &std::path::Path) -> Option<String> {
    const MAX_PARENT_HOPS: usize = 4;
    let mut dir = start.to_path_buf();
    for _ in 0..=MAX_PARENT_HOPS {
        if dir.join("idVendor").is_file() {
            return dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|n| !n.is_empty());
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => break,
        }
    }
    None
}

/// Parse the `Supported interface modes:` block out of `iw phy <phy> info`.
/// Each mode sits on its own `* <mode>` line under the header; the block ends at
/// the next non-`*` line. Returns the mode names (e.g. `managed`, `monitor`) in
/// listed order. Pure helper, unit-tested independently of `iw`.
#[cfg(any(target_os = "linux", test))]
fn parse_supported_modes(info: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_section = false;
    for line in info.lines() {
        let stripped = line.trim();
        if stripped.contains("Supported interface modes:") {
            in_section = true;
            continue;
        }
        if in_section {
            if let Some(rest) = stripped.strip_prefix("* ") {
                let mode = rest.trim();
                if !mode.is_empty() {
                    out.push(mode.to_string());
                }
            } else if !stripped.is_empty() && !stripped.starts_with('*') {
                // A non-bullet, non-blank line closes the modes section.
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
