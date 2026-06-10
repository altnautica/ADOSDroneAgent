//! WiFi adapter discovery, WFB-ng classification, and injection selection.
//!
//! Scans `/sys/class/net`, classifies each radio against the canonical WFB
//! compatibility table + driver-name fallback (denying management WiFi first),
//! ranks RTL-family adapters ahead of bus order, and proves the winner by
//! setting + reading back monitor mode. The pure classifier
//! (`build_adapter_info`) and the sysfs parent-walks are unit-testable off a
//! fixture tree without touching a real SBC.

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

/// The result returned to the radio manager.
#[derive(Debug, Clone)]
pub struct SelectedAdapter {
    pub ifname: String,
    pub chipset: String,
    pub injection_ok: bool,
    /// The selected adapter's enumerated USB link speed (Mbps), or `None` when
    /// not USB-backed / unreadable. Surfaced so the operator sees "12 Mbps".
    pub usb_speed_mbps: Option<u32>,
    /// True when the adapter enumerated on a USB link slower than high-speed
    /// (e.g. 12 Mbps full-speed on a flaky port / companion controller). Such an
    /// adapter can pass monitor-mode setup and advance its tx_bytes counter yet
    /// emit no usable RF — the agent must surface this rather than report a
    /// healthy link.
    pub usb_degraded: bool,
}

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
fn build_adapter_info(
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

/// The outcome of one adapter selection pass: the full detected list (for the
/// `wfb-adapters.json` sidecar + the no-injection diagnostic counts) and the
/// verified injection adapter if one could be proven.
#[derive(Debug, Clone)]
pub struct SelectionOutcome {
    /// Every detected WiFi adapter, compatible or not — written verbatim to the
    /// adapters sidecar so the GCS panel can render the full scan verdict.
    pub adapters: Vec<WifiAdapterInfo>,
    /// The verified injection adapter, or `None` when none could be proven.
    pub selected: Option<SelectedAdapter>,
}

impl SelectionOutcome {
    /// Total adapters detected (the loud no-injection log's `total_adapters`).
    pub fn total(&self) -> usize {
        self.adapters.len()
    }

    /// Adapters that are WFB-compatible AND advertise monitor mode (the loud
    /// no-injection log's `compatible` — the real injection-candidate count).
    pub fn compatible_monitor(&self) -> usize {
        self.adapters
            .iter()
            .filter(|a| a.is_wfb_compatible && a.supports_monitor)
            .count()
    }
}

/// Detect adapters and return the best verified RTL injection adapter, plus the
/// full detected list. The override iface (when set) is honoured verbatim. The
/// candidate set is the compatible+monitor adapters, ranked RTL-family-first so
/// bus order never decides, and each is proven by setting + reading back
/// monitor mode; the first that verifies wins. This is the full contract the
/// radio service uses (it needs the detected list for the adapters sidecar and
/// the scan counts for the no-injection diagnostic). Callers that only need the
/// selected iface use `select_interface`.
#[cfg(target_os = "linux")]
pub async fn detect_and_select(override_iface: &str) -> SelectionOutcome {
    let adapters = detect_wfb_adapters().await;

    if !override_iface.is_empty() {
        // Operator-specified interface: skip discovery ranking, still validate.
        let chipset = adapters
            .iter()
            .find(|a| a.interface_name == override_iface)
            .map(|a| a.chipset.clone())
            .unwrap_or_else(|| chipset_for_iface(override_iface));
        let ok = super::monitor::set_monitor_mode_verified(override_iface, 4).await;
        let speed = adapters
            .iter()
            .find(|a| a.interface_name == override_iface)
            .and_then(|a| a.usb_speed_mbps);
        let usb_degraded = usb_speed_degraded(speed);
        if usb_degraded {
            tracing::warn!(
                interface = %override_iface,
                usb_speed_mbps = ?speed,
                "wfb_adapter_usb_degraded: adapter on a slow USB link (needs 480 Mbps); RF may not transmit"
            );
        }
        return SelectionOutcome {
            adapters,
            selected: Some(SelectedAdapter {
                ifname: override_iface.to_string(),
                chipset,
                injection_ok: ok,
                usb_speed_mbps: speed,
                usb_degraded,
            }),
        };
    }

    // Candidates = compatible + monitor-capable, ranked RTL-family-first.
    let mut candidates: Vec<&WifiAdapterInfo> = adapters
        .iter()
        .filter(|a| a.is_wfb_compatible && a.supports_monitor)
        .collect();
    candidates.sort_by_key(|a| injection_rank(a));

    let mut selected = None;
    for adapter in candidates {
        tracing::info!(
            interface = %adapter.interface_name,
            chipset = %adapter.chipset,
            rank = injection_rank(adapter),
            "wfb_adapter_candidate"
        );
        if super::monitor::set_monitor_mode_verified(&adapter.interface_name, 4).await {
            let usb_degraded = usb_speed_degraded(adapter.usb_speed_mbps);
            if usb_degraded {
                tracing::warn!(
                    interface = %adapter.interface_name,
                    chipset = %adapter.chipset,
                    usb_speed_mbps = ?adapter.usb_speed_mbps,
                    "wfb_adapter_usb_degraded: selected adapter on a slow USB link (needs 480 Mbps); RF may not transmit"
                );
            } else {
                tracing::info!(
                    interface = %adapter.interface_name,
                    chipset = %adapter.chipset,
                    "wfb_adapter_selected"
                );
            }
            selected = Some(SelectedAdapter {
                ifname: adapter.interface_name.clone(),
                chipset: adapter.chipset.clone(),
                injection_ok: true,
                usb_speed_mbps: adapter.usb_speed_mbps,
                usb_degraded,
            });
            break;
        }
        tracing::warn!(
            interface = %adapter.interface_name,
            chipset = %adapter.chipset,
            "wfb_adapter_monitor_rejected"
        );
    }

    SelectionOutcome { adapters, selected }
}

#[cfg(not(target_os = "linux"))]
pub async fn detect_and_select(_override_iface: &str) -> SelectionOutcome {
    SelectionOutcome {
        adapters: Vec::new(),
        selected: None,
    }
}

/// Return the best verified RTL injection adapter, or `None` when none could be
/// proven. The thin selected-only seam other crates (the GS relay + receiver)
/// call; the radio service uses `detect_and_select` when it also needs the full
/// scan list + counts.
pub async fn select_interface(override_iface: &str) -> Option<SelectedAdapter> {
    detect_and_select(override_iface).await.selected
}

/// Rank an adapter so the validated RTL injection radios float to the front
/// (lower is better): RTL8812EU silicon first, RTL8812AU rebadges next, any
/// other passing chip last. Makes selection independent of USB bus order so a
/// management WiFi enumerated first can never win by accident. Mirrors the
/// Python `_injection_rank`.
#[cfg(any(target_os = "linux", test))]
fn injection_rank(adapter: &WifiAdapterInfo) -> u8 {
    let label = adapter.chipset.to_ascii_uppercase();
    let driver = adapter.driver.to_ascii_lowercase();
    let is_eu = label.contains("8812EU")
        || label.contains("88X2EU")
        || matches!(driver.as_str(), "8812eu" | "rtl8812eu" | "rtl88x2eu");
    let is_au = label.contains("8812AU")
        || label.contains("88XXAU")
        || matches!(driver.as_str(), "8812au" | "rtl8812au" | "rtl88xxau");
    let is_known = match (adapter.usb_vid, adapter.usb_pid) {
        (Some(v), Some(p)) => wfb_compatible_label(v, p).is_some(),
        _ => false,
    } || driver_is_wfb_compatible(&adapter.driver);
    if is_eu {
        0
    } else if is_au {
        1
    } else if is_known {
        2
    } else {
        3
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
fn wfb_compatible_label(vid: u16, pid: u16) -> Option<&'static str> {
    WFB_COMPATIBLE
        .iter()
        .find(|(v, p, _)| *v == vid && *p == pid)
        .map(|(_, _, label)| *label)
}

/// True when a driver name (lower-cased) is one of the known WFB-ng DKMS
/// modules. Authoritative even when the USB ID walk missed the IDs (e.g. a USB
/// hub layer hides the parent device).
#[cfg(any(target_os = "linux", test))]
fn driver_is_wfb_compatible(driver: &str) -> bool {
    let d = driver.trim().to_ascii_lowercase();
    WFB_COMPATIBLE_DRIVERS.contains(&d.as_str())
}

#[allow(dead_code)]
fn chipset_for_iface(_iface: &str) -> String {
    // For operator-specified overrides we skip the async classify call and
    // return a placeholder; the caller updates this from the heartbeat once
    // the interface is confirmed.
    "override".to_string()
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
mod tests {
    use super::*;

    #[test]
    fn deny_aic8800_by_driver() {
        assert!(is_denied("aic8800fwh", None));
        assert!(is_denied("brcmfmac", None));
    }

    #[test]
    fn deny_aic8800_by_vid() {
        assert!(is_denied("some_driver", Some((0xA69C, 0x1234))));
    }

    #[test]
    fn rtl8812eu_classified_rank_0() {
        let (label, rank) = classify("rtl88x2eu", None);
        assert_eq!(rank, 0);
        assert!(label.contains("RTL8812EU"));
    }

    #[test]
    fn rtl8812au_classified_rank_1() {
        let (label, rank) = classify("rtl8812au", None);
        assert_eq!(rank, 1);
        assert!(label.contains("RTL8812AU"));
    }

    #[test]
    fn vid_pid_a81a_disambiguated_by_driver() {
        // With EU driver, should be EU.
        let (label, rank) = classify("rtl88x2eu", Some((0x0BDA, 0xA81A)));
        assert_eq!(rank, 0);
        assert!(label.contains("EU"));
    }

    #[test]
    fn unknown_is_rank_3() {
        let (_, rank) = classify("unknown_wifi_driver", None);
        assert_eq!(rank, 3);
    }

    #[test]
    fn vid_pid_parent_walk_finds_ids_on_parent_device() {
        // Model the sysfs shape: the resolved `device` node is the USB
        // *interface* (`…:1.0`), which has bInterfaceClass but no id files; the
        // ids live on its parent USB *device* node. The walk must climb to it.
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("1-1");
        let interface = device.join("1-1:1.0");
        std::fs::create_dir_all(&interface).unwrap();
        // Interface node: a class file but NO ids (the trap that returned None).
        std::fs::write(interface.join("bInterfaceClass"), "ff\n").unwrap();
        // Parent device node: the real ids.
        std::fs::write(device.join("idVendor"), "0bda\n").unwrap();
        std::fs::write(device.join("idProduct"), "a81a\n").unwrap();

        assert_eq!(vid_pid_from_device_dir(&interface), Some((0x0BDA, 0xA81A)));
    }

    #[test]
    fn vid_pid_walk_returns_none_when_no_ancestor_has_ids() {
        // A tree with no id files anywhere within the bound yields None rather
        // than climbing out to the filesystem root.
        let dir = tempfile::tempdir().unwrap();
        let leaf = dir.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&leaf).unwrap();
        assert_eq!(vid_pid_from_device_dir(&leaf), None);
    }

    #[test]
    fn speed_parent_walk_reads_speed_off_the_device_node() {
        // `speed` lives on the USB device node (same as the ids), one level up
        // from the interface node the `device` symlink resolves to.
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("1-1");
        let interface = device.join("1-1:1.0");
        std::fs::create_dir_all(&interface).unwrap();
        std::fs::write(device.join("speed"), "480\n").unwrap();
        assert_eq!(speed_from_device_dir(&interface), Some(480));

        // A full-speed enumeration (the degraded RTL path) reads back as 12.
        let dir2 = tempfile::tempdir().unwrap();
        let dev2 = dir2.path().join("8-1");
        std::fs::create_dir_all(&dev2).unwrap();
        std::fs::write(dev2.join("speed"), "12\n").unwrap();
        assert_eq!(speed_from_device_dir(&dev2), Some(12));
    }

    #[test]
    fn usb_speed_degraded_flags_full_speed_only() {
        assert!(usb_speed_degraded(Some(12))); // full-speed RTL = degraded
        assert!(!usb_speed_degraded(Some(480))); // high-speed = fine
        assert!(!usb_speed_degraded(Some(5000))); // SuperSpeed = fine
        assert!(!usb_speed_degraded(None)); // non-USB / unknown is not flagged
    }

    #[test]
    fn parse_supported_modes_extracts_bulleted_modes() {
        let info = "\
Wiphy phy0
	Supported interface modes:
		 * IBSS
		 * managed
		 * monitor
	Band 1:
		Frequencies:
";
        let modes = parse_supported_modes(info);
        assert_eq!(modes, vec!["IBSS", "managed", "monitor"]);
    }

    #[test]
    fn parse_supported_modes_absent_is_empty() {
        assert!(parse_supported_modes("Wiphy phy0\n\tBand 1:\n").is_empty());
        assert!(parse_supported_modes("").is_empty());
    }

    // ── VID:PID classification table ─────────────────────────────────────────

    #[test]
    fn vid_pid_table_matches_python_compatible_set() {
        // Every (VID, PID) the Python WFB_COMPATIBLE table carries must classify
        // as a known WFB adapter with the exact same label.
        let cases: &[(u16, u16, &str)] = &[
            (0x0BDA, 0x8812, "RTL8812AU"),
            (0x0BDA, 0x881A, "RTL8812AU (alt)"),
            (0x0BDA, 0x881B, "RTL8812AU (alt)"),
            (0x0BDA, 0x881C, "RTL8812AU (alt)"),
            (0x0BDA, 0xA81A, "RTL8812AU (a81a)"),
            (0x0BDA, 0xB812, "RTL8812EU"),
            (0x2357, 0x0120, "RTL8812AU (TP-Link)"),
            (0x2357, 0x0101, "RTL8812AU (TP-Link alt)"),
        ];
        for (vid, pid, label) in cases {
            assert_eq!(
                wfb_compatible_label(*vid, *pid),
                Some(*label),
                "table miss for {vid:#06x}:{pid:#06x}"
            );
        }
        // A PID outside the table is not known.
        assert_eq!(wfb_compatible_label(0x0BDA, 0x0000), None);
        // The old wrong 0x8812 EU label is gone — 0x8812 is the AU rebadge.
        assert_eq!(wfb_compatible_label(0x0BDA, 0x8812), Some("RTL8812AU"));
    }

    #[test]
    fn build_adapter_info_rtl_eu_driver_is_compatible() {
        let a = build_adapter_info(
            "wlan1".to_string(),
            "rtl88x2eu".to_string(),
            Some((0x0BDA, 0xB812)),
            "phy0".to_string(),
            vec!["managed".to_string(), "monitor".to_string()],
            true,
            Some("managed".to_string()),
        );
        assert!(a.is_wfb_compatible);
        assert_eq!(a.chipset, "RTL8812EU");
        assert_eq!(a.usb_vid, Some(0x0BDA));
        assert_eq!(a.usb_pid, Some(0xB812));
        assert!(a.supports_monitor);
    }

    #[test]
    fn build_adapter_info_a81a_promoted_to_eu_by_driver() {
        // The ambiguous A81A PID becomes the EU label when the EU driver bound.
        let a = build_adapter_info(
            "wlan1".to_string(),
            "rtl88x2eu".to_string(),
            Some((0x0BDA, 0xA81A)),
            "phy0".to_string(),
            vec!["monitor".to_string()],
            true,
            None,
        );
        assert!(a.is_wfb_compatible);
        assert_eq!(a.chipset, "RTL8812EU (a81a)");
        // With a non-EU driver it keeps the default AU rebadge label.
        let b = build_adapter_info(
            "wlan2".to_string(),
            "88XXau".to_string(),
            Some((0x0BDA, 0xA81A)),
            "phy1".to_string(),
            vec!["monitor".to_string()],
            true,
            None,
        );
        assert_eq!(b.chipset, "RTL8812AU (a81a)");
    }

    #[test]
    fn build_adapter_info_denies_aic8800() {
        // The management WiFi can never be flagged compatible, by VID or driver.
        let a = build_adapter_info(
            "wlan0".to_string(),
            "aic8800_fdrv".to_string(),
            Some((0xA69C, 0x8800)),
            "phy0".to_string(),
            vec!["managed".to_string(), "monitor".to_string()],
            true,
            Some("managed".to_string()),
        );
        assert!(!a.is_wfb_compatible);
        // Driver-only deny (no USB ID resolved) still denies.
        let b = build_adapter_info(
            "wlan0".to_string(),
            "brcmfmac".to_string(),
            None,
            String::new(),
            vec![],
            false,
            None,
        );
        assert!(!b.is_wfb_compatible);
    }

    #[test]
    fn build_adapter_info_driver_fallback_when_usb_id_missing() {
        // A USB hub layout can hide the parent IDs; the known DKMS driver name
        // is enough to flag compatibility.
        let a = build_adapter_info(
            "wlan1".to_string(),
            "8812eu".to_string(),
            None,
            "phy0".to_string(),
            vec!["monitor".to_string()],
            true,
            None,
        );
        assert!(a.is_wfb_compatible);
        assert_eq!(a.usb_vid, None);
        assert_eq!(a.usb_pid, None);
    }

    #[test]
    fn build_adapter_info_unknown_is_not_compatible() {
        let a = build_adapter_info(
            "wlan3".to_string(),
            "some_other_wifi".to_string(),
            Some((0x1234, 0x5678)),
            "phy0".to_string(),
            vec!["managed".to_string()],
            false,
            Some("managed".to_string()),
        );
        assert!(!a.is_wfb_compatible);
        assert_eq!(a.chipset, "some_other_wifi");
    }

    #[test]
    fn injection_rank_orders_eu_before_au_before_known_before_other() {
        let eu = build_adapter_info(
            "wlan1".to_string(),
            "rtl88x2eu".to_string(),
            Some((0x0BDA, 0xB812)),
            "phy0".to_string(),
            vec!["monitor".to_string()],
            true,
            None,
        );
        let au = build_adapter_info(
            "wlan2".to_string(),
            "rtl8812au".to_string(),
            Some((0x0BDA, 0x8812)),
            "phy1".to_string(),
            vec!["monitor".to_string()],
            true,
            None,
        );
        assert_eq!(injection_rank(&eu), 0);
        assert_eq!(injection_rank(&au), 1);
    }

    // ── WifiAdapterInfo JSON serialization (the adapter contract wire shape) ──

    #[test]
    fn wifi_adapter_info_serializes_with_python_field_names() {
        let info = WifiAdapterInfo {
            interface_name: "wlan1".to_string(),
            driver: "rtl88x2eu".to_string(),
            chipset: "RTL8812EU".to_string(),
            supports_monitor: true,
            current_mode: Some("monitor".to_string()),
            phy: "phy0".to_string(),
            usb_vid: Some(0x0BDA),
            usb_pid: Some(0xB812),
            usb_speed_mbps: Some(480),
            is_wfb_compatible: true,
            capabilities: vec!["managed".to_string(), "monitor".to_string()],
        };
        let v = serde_json::to_value(&info).unwrap();
        assert_eq!(v["interface_name"], "wlan1");
        assert_eq!(v["driver"], "rtl88x2eu");
        assert_eq!(v["chipset"], "RTL8812EU");
        assert_eq!(v["supports_monitor"], true);
        assert_eq!(v["current_mode"], "monitor");
        assert_eq!(v["phy"], "phy0");
        assert_eq!(v["usb_vid"], 0x0BDA);
        assert_eq!(v["usb_pid"], 0xB812);
        assert_eq!(v["usb_speed_mbps"], 480);
        assert_eq!(v["is_wfb_compatible"], true);
        assert_eq!(v["capabilities"], serde_json::json!(["managed", "monitor"]));
        // The contract carries the adapter facts the panel + REST read.
        assert_eq!(v.as_object().unwrap().len(), 11);
    }

    #[test]
    fn wifi_adapter_info_unbacked_iface_nulls_mode_and_usb() {
        let info = WifiAdapterInfo {
            interface_name: "wlan9".to_string(),
            driver: "unknown".to_string(),
            chipset: "unknown".to_string(),
            supports_monitor: false,
            current_mode: None,
            phy: String::new(),
            usb_vid: None,
            usb_pid: None,
            usb_speed_mbps: None,
            is_wfb_compatible: false,
            capabilities: vec![],
        };
        let v = serde_json::to_value(&info).unwrap();
        // Missing facts are JSON null, not omitted, so the panel can distinguish
        // "not USB-backed" from "unread".
        assert!(v["current_mode"].is_null());
        assert!(v["usb_vid"].is_null());
        assert!(v["usb_pid"].is_null());
    }
}
