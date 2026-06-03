//! RTL8812EU adapter selection and monitor-mode setup.
//!
//! Mirrors `services/wfb/adapter.py`: scans network interfaces, identifies
//! RTL injection radios by USB VID/PID or driver name, denies AIC8800 and
//! brcmfmac management-wifi adapters, validates monitor mode with a 4× retry.
//!
//! All OS calls (iw, nmcli, ip) are Linux-only. On non-Linux hosts this
//! module compiles but `select_interface` always returns None.

use serde::Serialize;

/// Known WFB-ng compatible chipsets by (VID, PID) → label. The RTL8812AU
/// family (0x8812, 0x881A-C), RTL8812EU / RTL8822E (0xB812 / 0xA81A), and the
/// TP-Link rebadges all share the same vendored DKMS driver and support
/// monitor mode with frame injection. PID 0xA81A is ambiguous — it ships on
/// both AU rebadges and EU dongles, so the bound kernel driver disambiguates
/// (the `classify` path promotes it to the EU label when the driver is the EU
/// module). This is the single canonical copy; it must match the Python
/// `WFB_COMPATIBLE` table byte-for-byte.
#[cfg(any(target_os = "linux", test))]
const WFB_COMPATIBLE: &[(u16, u16, &str)] = &[
    (0x0BDA, 0x8812, "RTL8812AU"),
    (0x0BDA, 0x881A, "RTL8812AU (alt)"),
    (0x0BDA, 0x881B, "RTL8812AU (alt)"),
    (0x0BDA, 0x881C, "RTL8812AU (alt)"),
    (0x0BDA, 0xA81A, "RTL8812AU (a81a)"),
    (0x0BDA, 0xB812, "RTL8812EU"),
    (0x2357, 0x0120, "RTL8812AU (TP-Link)"),
    (0x2357, 0x0101, "RTL8812AU (TP-Link alt)"),
];

/// Driver-name fallback for boards whose VID:PID is not yet in the table
/// above. The DKMS module exposes itself under one of these names; if any
/// matches, the adapter is treated as WFB-ng compatible regardless of the USB
/// ID lookup, so future Realtek rebadges work without a table edit.
#[cfg(any(target_os = "linux", test))]
const WFB_COMPATIBLE_DRIVERS: &[&str] = &[
    "8812au",
    "8812eu",
    "rtl8812au",
    "rtl8812eu",
    "rtl88x2eu",
    "rtl88xxau",
];

/// VID deny-set: AIC8800 management-only adapters must never be used. They
/// advertise monitor mode but cannot inject 802.11 frames, so wfb_tx/wfb_rx on
/// them produces zero link even when `iw set monitor` reports success.
#[cfg(any(target_os = "linux", test))]
const DENY_VID: &[u16] = &[0xA69C];

/// Driver prefix deny-set: AIC8800 (Rock 5C onboard management WiFi) and
/// Broadcom FullMAC (Pi-family onboard). Denied first so neither the USB-ID
/// match nor the driver-name fallback can flip them WFB-compatible.
#[cfg(any(target_os = "linux", test))]
const DENY_DRIVER_PREFIXES: &[&str] = &["aic8800", "brcmfmac"];

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
    let control_iface = control_interface().await;
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
        let current_mode = get_interface_mode(&name).await;
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
        let ok = set_monitor_mode_verified(override_iface, 4).await;
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
        if set_monitor_mode_verified(&adapter.interface_name, 4).await {
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
    match run_cmd_output("iw", &["phy", phy, "info"]).await {
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

/// Parse the iface carrying the kernel default route out of `ip route` output.
/// Returns the first `dev <iface>` on a `default ...` line, or `None`.
fn parse_default_route_iface(text: &str) -> Option<String> {
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.first() == Some(&"default") {
            if let Some(idx) = parts.iter().position(|p| *p == "dev") {
                if let Some(iface) = parts.get(idx + 1) {
                    return Some((*iface).to_string());
                }
            }
        }
    }
    None
}

/// Return the interface carrying the kernel default route, or `None`.
///
/// This is the operator's control path (the iface their SSH / Mission Control
/// session arrives over). The radio adapter selection and monitor-mode setup
/// must never touch it: bringing it down or flipping it to monitor mode would
/// sever the only management link with no fallback and strand the box. Best
/// effort — a missing default route (isolated rig) returns `None`.
async fn control_interface() -> Option<String> {
    match run_cmd_output("ip", &["-4", "route", "show", "default"]).await {
        Ok(out) => parse_default_route_iface(&out),
        Err(()) => None,
    }
}

/// Set monitor mode on an interface and verify the readback. Retries up to
/// `max_retries` times with 500ms backoff. Returns true only on verified
/// monitor mode.
///
/// Sequence (mirrors adapter.py:471-561):
///   nmcli dev set <if> managed no  (best-effort)
///   ip link set <if> down
///   iw <if> set type monitor        (primary)
///     fallback: iw <if> set monitor none  (EIO workaround for Rock 5C 8812eu)
///   ip link set <if> up
///   iw dev <if> set power_save off  (best-effort)
///   verify: iw <if> info | grep "type monitor"
pub async fn set_monitor_mode_verified(iface: &str, max_retries: u32) -> bool {
    for attempt in 0..max_retries.max(1) {
        if attempt > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(500 * attempt as u64)).await;
        }
        if set_monitor_mode(iface).await && verify_monitor_mode(iface).await {
            return true;
        }
    }
    false
}

async fn set_monitor_mode(iface: &str) -> bool {
    // Defense in depth: never flip the operator's control path to monitor mode.
    // Bringing it down or changing its type would sever the only management link.
    if control_interface().await.as_deref() == Some(iface) {
        tracing::error!(iface = %iface, "monitor_mode_refused_control_iface");
        return false;
    }
    // Best-effort: release from NetworkManager.
    let _ = run_cmd("nmcli", &["dev", "set", iface, "managed", "no"]).await;
    // Bring down.
    if run_cmd("ip", &["link", "set", iface, "down"])
        .await
        .is_err()
    {
        return false;
    }
    // Monitor-form order is load-bearing on the RTL8812EU: `iw <if> set type
    // monitor` returns success on this chipset yet leaves the PHY pinned at the
    // muted txpower floor (readback -100 dBm, carrier down, every injection
    // sendmsg fails ENOBUFS), whereas `iw dev <if> set monitor none` fully
    // initialises the monitor PHY so it radiates. So the monitor-flags form is
    // primary (it is also the EIO workaround seen on other 8812eu builds); the
    // type form is the fallback for any adapter that rejects the flags form.
    let set_ok = if run_cmd("iw", &["dev", iface, "set", "monitor", "none"])
        .await
        .is_ok()
    {
        true
    } else {
        run_cmd("iw", &[iface, "set", "type", "monitor"])
            .await
            .is_ok()
    };
    if !set_ok {
        return false;
    }
    // Bring up.
    if run_cmd("ip", &["link", "set", iface, "up"]).await.is_err() {
        return false;
    }
    // Best-effort: disable power-save.
    let _ = run_cmd("iw", &["dev", iface, "set", "power_save", "off"]).await;
    true
}

async fn verify_monitor_mode(iface: &str) -> bool {
    get_interface_mode(iface).await.as_deref() == Some("monitor")
}

/// Parse the operating mode out of `iw <iface> info` output. Returns the value
/// after the `type ` line ("monitor" | "managed" | …), or `None` when the mode
/// cannot be determined. Pure helper, unit-tested independently of `iw`.
fn parse_interface_mode(info: &str) -> Option<String> {
    for line in info.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("type ") {
            let mode = rest.trim();
            if !mode.is_empty() {
                return Some(mode.to_string());
            }
        }
    }
    None
}

/// Return the interface operating mode ("monitor" | "managed" | …) by reading
/// `iw <iface> info`, or `None` when it cannot be read. Used to verify monitor
/// mode took effect and to detect when a scan stranded the iface in managed
/// mode (a managed-mode iface injects zero frames even though every command
/// "succeeded").
pub async fn get_interface_mode(iface: &str) -> Option<String> {
    let out = run_cmd_output("iw", &[iface, "info"]).await.ok()?;
    parse_interface_mode(&out)
}

/// Re-assert monitor mode and retune the current channel when a scan left the
/// iface in a non-monitor state.
///
/// `iw scan` can leave some RTL drivers in managed mode, which silently stops
/// `wfb_tx` injection while the link is still nominally up. After every scan the
/// hop loop calls this: when the observed mode is not monitor (and is readable),
/// it puts the iface back into monitor mode and retunes to `current_channel` so
/// `wfb_tx` keeps injecting on the channel it expects. When the mode is already
/// monitor (or unreadable) this is a cheap no-op.
pub async fn restore_monitor_if_needed(iface: &str, current_channel: u8) {
    match get_interface_mode(iface).await {
        Some(mode) if mode == "monitor" => {}
        None => {}
        Some(observed) => {
            tracing::warn!(iface, observed_mode = %observed, "monitor_restored_after_scan");
            if set_monitor_mode(iface).await {
                let _ = run_cmd(
                    "iw",
                    &[iface, "set", "channel", &current_channel.to_string()],
                )
                .await;
            }
        }
    }
}

/// The 5 GHz channel numbers this adapter can actually use for the link.
///
/// Parses `iw <iface> info` to find the wiphy, then `iw phy <phyN> channels`,
/// keeping only channels that are not `(disabled)` and not radar / `no IR` (DFS
/// channels need a channel-availability check the link does not perform). The
/// drone and ground frequently run different regulatory domains, so the air
/// channel must be in the intersection of both sides' enabled sets; this exposes
/// the local half. An empty set means "could not determine"; callers treat that
/// as "do not restrict".
#[cfg(target_os = "linux")]
pub async fn enabled_channels(iface: &str) -> std::collections::BTreeSet<u8> {
    let info = match run_cmd_output("iw", &[iface, "info"]).await {
        Ok(out) => out,
        Err(()) => return std::collections::BTreeSet::new(),
    };
    let Some(phy) = parse_wiphy(&info) else {
        return std::collections::BTreeSet::new();
    };
    let chans = match run_cmd_output("iw", &["phy", &phy, "channels"]).await {
        Ok(out) => out,
        Err(()) => return std::collections::BTreeSet::new(),
    };
    parse_enabled_channels(&chans)
}

#[cfg(not(target_os = "linux"))]
pub async fn enabled_channels(_iface: &str) -> std::collections::BTreeSet<u8> {
    std::collections::BTreeSet::new()
}

/// Extract the `phyN` wiphy name from `iw <iface> info` output (the `wiphy <N>`
/// line). Returns e.g. `"phy0"`, or `None` when absent.
#[cfg(any(target_os = "linux", test))]
fn parse_wiphy(info: &str) -> Option<String> {
    for line in info.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("wiphy ") {
            let n = rest.split_whitespace().next()?;
            if n.chars().all(|c| c.is_ascii_digit()) && !n.is_empty() {
                return Some(format!("phy{}", n));
            }
        }
    }
    None
}

/// Parse `iw phy <phy> channels` output into the set of usable channel numbers.
/// A line carries a `[<channel>]` token; it is kept only when the line is not
/// marked `disabled`, `no ir`, or `radar`.
#[cfg(any(target_os = "linux", test))]
fn parse_enabled_channels(text: &str) -> std::collections::BTreeSet<u8> {
    let mut out = std::collections::BTreeSet::new();
    for line in text.lines() {
        // The channel number sits inside square brackets, e.g.
        //   "* 5745 MHz [149]"                       (usable)
        //   "* 5180 MHz [36] (disabled)"             (skip)
        //   "* 5260 MHz [52] (no IR, radar detection)" (skip)
        let Some(start) = line.find('[') else {
            continue;
        };
        let Some(len) = line[start + 1..].find(']') else {
            continue;
        };
        let token = &line[start + 1..start + 1 + len];
        let Ok(ch) = token.parse::<u8>() else {
            continue;
        };
        let low = line.to_lowercase();
        if low.contains("disabled") || low.contains("no ir") || low.contains("radar") {
            continue;
        }
        out.insert(ch);
    }
    out
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

/// True when `domain` is a well-formed regulatory domain code: exactly two
/// characters, each an uppercase ASCII letter or digit (`/^[A-Z0-9]{2}$/`).
/// Pure so the format gate is unit-testable without `iw`.
fn is_valid_reg_domain(domain: &str) -> bool {
    domain.len() == 2
        && domain
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// A failed regulatory-domain precondition. The radio bring-up treats this as a
/// hard precondition: the interface is never brought into monitor mode and no
/// channel is set while one of these holds, so the driver can never radiate on a
/// band the active domain forbids (the silent power-cap class).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegError {
    /// The `iw reg set` command failed to run or returned non-zero.
    CommandFailed,
    /// The domain string is not a 2-char ISO 3166-1 alpha-2 / `00` world code.
    InvalidFormat,
    /// After the bounded retries, `iw reg get` never reported the wanted domain.
    /// `got` is the last-observed global country, when readable.
    VerifyTimeout { want: String, got: Option<String> },
    /// A self-managed phy carries a baked country that overrides the global set:
    /// the global `iw reg set` cannot displace it, so the radio would run capped
    /// on the wanted band. Surfaced distinctly so the operator sees the conflict
    /// rather than a silently power-capped link.
    EepromOverride { want: String, got: String },
    /// The rendezvous channel is not in the domain's enabled channel set.
    ChannelNotEnabled { channel: u8 },
    /// The rendezvous channel needs DFS clearance and `dfs_allowed` is off.
    ChannelIsDfs { channel: u8 },
}

impl std::fmt::Display for RegError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegError::CommandFailed => write!(f, "reg command failed or unavailable"),
            RegError::InvalidFormat => write!(f, "invalid regulatory domain format"),
            RegError::VerifyTimeout { want, got } => {
                write!(f, "reg domain verify timeout (want={want}, got={got:?})")
            }
            RegError::EepromOverride { want, got } => {
                write!(f, "phy override (want={want}, got={got})")
            }
            RegError::ChannelNotEnabled { channel } => {
                write!(f, "channel {channel} not enabled in this domain")
            }
            RegError::ChannelIsDfs { channel } => {
                write!(f, "channel {channel} needs DFS clearance")
            }
        }
    }
}

impl std::error::Error for RegError {}

impl RegError {
    /// A short, stable token for the wfb-stats `reg_block_reason` field and the
    /// structured log. Bland and reader-facing; no internal identifiers.
    pub fn reason_code(&self) -> &'static str {
        match self {
            RegError::CommandFailed => "command_failed",
            RegError::InvalidFormat => "invalid_format",
            RegError::VerifyTimeout { .. } => "verify_timeout",
            RegError::EepromOverride { .. } => "phy_override",
            RegError::ChannelNotEnabled { .. } => "channel_not_enabled",
            RegError::ChannelIsDfs { .. } => "channel_dfs",
        }
    }
}

/// Number of `iw reg set` attempts before the gate declares a verify timeout.
const REG_SET_MAX_ATTEMPTS: u32 = 3;
/// Pause between reg-set attempts. With 3 attempts this spans ~6 s, matching the
/// bounded-retry budget; the per-attempt readback poll adds up to ~2 s each.
const REG_SET_RETRY_INTERVAL_MS: u64 = 2000;
/// Ceiling on the per-attempt `iw reg get` readback poll (the set is async).
const REG_VERIFY_POLL_CEILING_MS: u64 = 2000;
/// Cadence of the per-attempt readback poll.
const REG_VERIFY_POLL_STEP_MS: u64 = 100;

/// Apply the regulatory domain via `iw reg set <domain>` and verify the readback
/// with bounded retry. Returns `Ok(())` only when `iw reg get` reports the wanted
/// global country.
///
/// This is a hard precondition for the radio bring-up. It must run BEFORE the
/// interface is brought up in monitor mode: the kernel maps the permitted channel
/// set and the per-channel TX-power ceiling when the driver initialises, so a
/// domain set afterwards is too late and leaves the home channel (149, U-NII-3 /
/// 5745 MHz) capped to the startup domain's limits (the -100 dBm "not permitted"
/// sentinel, zero injected frames).
///
/// On an empty `domain` this is a no-op (`Ok(())`) — the caller opted out of
/// setting one. A malformed domain returns `InvalidFormat`. After
/// [`REG_SET_MAX_ATTEMPTS`] failed verifications it returns `VerifyTimeout`. When
/// a self-managed phy re-asserts a baked country that overrides the global set, it
/// returns `EepromOverride` instead of silently running capped.
///
/// This never touches an interface — `iw reg set` is a global per-phy call — so
/// it cannot disturb the operator's management link.
pub async fn set_reg_domain(domain: &str) -> Result<(), RegError> {
    if domain.is_empty() {
        return Ok(());
    }
    // An ISO 3166-1 alpha-2 country / `00` world domain is exactly two chars,
    // each `A-Z` or `0-9`. Reject anything else before it reaches `iw reg set`
    // so a malformed value (stray whitespace, a full name, an injected token)
    // is never handed to the command.
    if !is_valid_reg_domain(domain) {
        tracing::warn!(domain, "wfb_reg_domain_rejected_format");
        return Err(RegError::InvalidFormat);
    }
    let want = domain.to_ascii_uppercase();
    let mut cmd_ran_at_least_once = false;
    for attempt in 0..REG_SET_MAX_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(
                REG_SET_RETRY_INTERVAL_MS,
            ))
            .await;
        }
        if run_cmd("iw", &["reg", "set", &want]).await.is_err() {
            tracing::warn!(domain = %want, attempt, "wfb_reg_set_cmd_failed");
            continue;
        }
        cmd_ran_at_least_once = true;
        // Poll the readback for this attempt.
        let deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_millis(REG_VERIFY_POLL_CEILING_MS);
        loop {
            if active_global_reg_domain().await.as_deref() == Some(want.as_str()) {
                tracing::info!(domain = %want, verified = true, "wfb_reg_domain_verified");
                return Ok(());
            }
            // A self-managed phy that re-asserts a different baked country is an
            // unrecoverable conflict, not a timing issue — fail fast and loud.
            if let Some((phy, baked)) = first_conflicting_self_managed_phy(&want).await {
                tracing::error!(
                    want = %want,
                    got = %baked,
                    phy = %phy,
                    "wfb_reg_phy_override"
                );
                return Err(RegError::EepromOverride { want, got: baked });
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(REG_VERIFY_POLL_STEP_MS)).await;
        }
    }
    if !cmd_ran_at_least_once {
        tracing::error!(domain = %want, "wfb_reg_set_unavailable");
        return Err(RegError::CommandFailed);
    }
    let got = active_global_reg_domain().await;
    tracing::error!(want = %want, got = ?got, "wfb_reg_domain_verify_timeout");
    Err(RegError::VerifyTimeout { want, got })
}

/// Validate that the radio is clear to bring up on `channel`: the channel must be
/// in the domain's enabled set, and (unless `dfs_allowed`) must not be a DFS
/// channel. Call after [`set_reg_domain`] succeeds and after `enabled_channels`
/// has been read for the interface.
///
/// `enabled` is the regulatory-permitted set from [`enabled_channels`], which
/// already filters DFS / no-IR / disabled channels. An empty set means the wiphy
/// list could not be read; the gate treats that as "could not determine" and
/// passes (matching the existing "empty = do not restrict" convention) so a board
/// whose channel list is unreadable still comes up rather than wedging.
///
/// `dfs_channels` is the set of channels the same readout flagged as needing DFS
/// clearance. When the rendezvous channel sits in that set and `dfs_allowed` is
/// off, this returns `ChannelIsDfs` so a DFS home is refused at preflight.
pub fn assert_reg_ready(
    channel: u8,
    enabled: &std::collections::BTreeSet<u8>,
    dfs_channels: &std::collections::BTreeSet<u8>,
    dfs_allowed: bool,
) -> Result<(), RegError> {
    // Could not read the wiphy channel list: do not restrict (the radio may
    // still come up on a permissive driver). Never wedge on unknown.
    if enabled.is_empty() {
        return Ok(());
    }
    if !dfs_allowed && dfs_channels.contains(&channel) {
        return Err(RegError::ChannelIsDfs { channel });
    }
    if !enabled.contains(&channel) {
        return Err(RegError::ChannelNotEnabled { channel });
    }
    Ok(())
}

/// The DFS / no-IR / radar channels for this interface's domain — the channels a
/// rendezvous home must avoid unless `dfs_allowed`. Reads the same
/// `iw phy <phy> channels` output as [`enabled_channels`] and keeps the channels
/// it marks `no ir` / `radar`. An empty set means "could not determine".
#[cfg(target_os = "linux")]
pub async fn dfs_channels(iface: &str) -> std::collections::BTreeSet<u8> {
    let info = match run_cmd_output("iw", &[iface, "info"]).await {
        Ok(out) => out,
        Err(()) => return std::collections::BTreeSet::new(),
    };
    let Some(phy) = parse_wiphy(&info) else {
        return std::collections::BTreeSet::new();
    };
    let chans = match run_cmd_output("iw", &["phy", &phy, "channels"]).await {
        Ok(out) => out,
        Err(()) => return std::collections::BTreeSet::new(),
    };
    parse_dfs_channels(&chans)
}

#[cfg(not(target_os = "linux"))]
pub async fn dfs_channels(_iface: &str) -> std::collections::BTreeSet<u8> {
    std::collections::BTreeSet::new()
}

/// Parse `iw phy <phy> channels` into the set of channels that need DFS
/// clearance (lines marked `no ir` or `radar`). A `disabled` channel is not a
/// DFS channel — it is simply unavailable — so it is excluded here.
#[cfg(any(target_os = "linux", test))]
fn parse_dfs_channels(text: &str) -> std::collections::BTreeSet<u8> {
    let mut out = std::collections::BTreeSet::new();
    for line in text.lines() {
        let Some(start) = line.find('[') else {
            continue;
        };
        let Some(len) = line[start + 1..].find(']') else {
            continue;
        };
        let token = &line[start + 1..start + 1 + len];
        let Ok(ch) = token.parse::<u8>() else {
            continue;
        };
        let low = line.to_lowercase();
        if low.contains("disabled") {
            continue;
        }
        if low.contains("no ir") || low.contains("radar") {
            out.insert(ch);
        }
    }
    out
}

/// Return the first self-managed phy whose baked country differs from
/// `global_want`, or `None`. This is the unrecoverable EEPROM-override case: a
/// global `iw reg set` cannot displace a self-managed phy's baked country, so the
/// radio on that phy would run capped on the wanted band.
async fn first_conflicting_self_managed_phy(global_want: &str) -> Option<(String, String)> {
    let out = run_cmd_output("iw", &["reg", "get"]).await.ok()?;
    parse_conflicting_self_managed_phy(&out, global_want)
}

/// Pure parser for the EEPROM-override detection. Walks `iw reg get` output: a
/// `phyN (self-managed)` header opens a block, and the first `country XX:` line
/// inside it is that phy's baked country. Returns the first `(phy, country)` whose
/// country differs from `global_want`. Tolerant of the `self managed` /
/// `self-managed` spelling variants `iw` has used. Pure so it is unit-testable
/// without `iw`.
fn parse_conflicting_self_managed_phy(text: &str, global_want: &str) -> Option<(String, String)> {
    let want = global_want.to_ascii_uppercase();
    let mut current_phy: Option<String> = None;
    for line in text.lines() {
        let s = line.trim();
        let low = s.to_lowercase();
        // A self-managed phy block header, e.g. "phy#3 (self-managed)" or
        // "phy3 (self managed)". The phy token may carry a '#'.
        if low.starts_with("phy") && (low.contains("self managed") || low.contains("self-managed"))
        {
            let raw = s.split_whitespace().next().unwrap_or("");
            let phy = raw.trim_start_matches("phy#").trim_start_matches("phy");
            current_phy = Some(format!("phy{phy}"));
            continue;
        }
        if let Some(rest) = s.strip_prefix("country ") {
            let cc: String = rest
                .chars()
                .take(2)
                .collect::<String>()
                .to_ascii_uppercase();
            if cc.len() == 2 {
                if let Some(phy) = current_phy.take() {
                    if cc != want {
                        return Some((phy, cc));
                    }
                }
            }
        }
    }
    None
}

/// The regulatory domain actually in force plus whether it matches the wanted
/// domain. `domain` is the live global country from `iw reg get` (e.g. `US`,
/// `BO`, `00`), or `None` when it could not be read. `verified` is true only
/// when the live domain equals `want` (case-insensitive). Surfaced on the
/// wfb-stats sidecar so a future regression (a forbidden domain the global set
/// could not displace) is visible in one glance instead of masked by a
/// configured-channel-and-locked report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegStatus {
    pub domain: Option<String>,
    pub verified: bool,
}

/// Read the live regulatory domain and report whether it matches `want`. A
/// read-only `iw reg get` call; it never touches an interface, so it cannot
/// disturb the operator's management link. `want` is the domain the gate asked
/// for (the resolved `reg_domain`); an empty `want` reports the live domain with
/// `verified=false` (nothing to match against).
pub async fn read_reg_status(want: &str) -> RegStatus {
    let domain = active_global_reg_domain().await;
    let verified = reg_is_verified(domain.as_deref(), want);
    RegStatus { domain, verified }
}

/// Pure verification decision: true only when a known live `domain` equals the
/// wanted domain (case-insensitive) and `want` is non-empty. Split out from
/// [`read_reg_status`] so the match logic is testable without `iw`.
fn reg_is_verified(domain: Option<&str>, want: &str) -> bool {
    !want.is_empty()
        && domain
            .map(|d| d.eq_ignore_ascii_case(want))
            .unwrap_or(false)
}

/// The outcome of one regulatory-domain reconcile attempt. Returned so the
/// caller can emit the durable `radio.reg_reasserted` event with the from/to and
/// the channel-safety result without re-reading any state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReassertOutcome {
    /// The live global domain already equalled the wanted domain: no action.
    InSync,
    /// The wanted domain was empty / malformed / the world default, so there was
    /// nothing safe to force.
    NoWanted,
    /// The wanted domain would not permit the configured channel, so forcing it
    /// would cap the radio. The re-assert was skipped.
    SkippedChannelUnsafe,
    /// The wanted domain was re-asserted. Carries the from/to countries and
    /// whether `iw reg set` verified (false = the set was issued but the readback
    /// did not confirm within the bounded retry, e.g. an EEPROM-override that the
    /// global set cannot displace — still worth recording the attempt).
    Reasserted {
        from: Option<String>,
        to: String,
        verified: bool,
    },
}

/// Reconcile the GLOBAL regulatory domain back to the configured `wanted` value,
/// re-asserting it when a self-managed injection PHY has left a different baked
/// country (e.g. `BO`) as the effective global domain.
///
/// This is the PREVENTION layer for the onboard-WiFi data-path break: a normal
/// onboard FullMAC adapter obeys the global domain, and when an injection PHY's
/// baked country becomes the global domain the onboard WiFi can keep its
/// association yet lose its data path. Re-asserting the sane wanted domain keeps
/// the onboard link working. The reactive WiFi self-heal stays as the backstop.
///
/// Safety: the re-assert is gated on the wanted domain PERMITTING the configured
/// `channel`. The caller passes the channel-vs-domain validation already used by
/// the bring-up gate (`assert_reg_ready` over the interface's `enabled_channels`
/// / `dfs_channels`), so this can never force a domain that caps the radio onto a
/// forbidden frequency. The world default (`00`) and any malformed domain are
/// refused. The call is idempotent — a no-op when the live domain already equals
/// the wanted value.
///
/// `channel_permitted_by_wanted` is the precomputed result of the channel gate
/// under the wanted domain; the caller computes it once (it already reads the
/// enabled set for the bring-up) and hands it in so this function does not repeat
/// the `iw phy channels` read. Returns the [`ReassertOutcome`] for the event.
pub async fn reconcile_reg_domain(
    wanted: &str,
    channel: u8,
    channel_permitted_by_wanted: bool,
) -> ReassertOutcome {
    let live = active_global_reg_domain().await;
    match crate::reg_reassert::reconcile_decision(
        live.as_deref(),
        wanted,
        channel_permitted_by_wanted,
    ) {
        crate::reg_reassert::ReassertDecision::InSync => ReassertOutcome::InSync,
        crate::reg_reassert::ReassertDecision::NoWanted => ReassertOutcome::NoWanted,
        crate::reg_reassert::ReassertDecision::SkipChannelUnsafe => {
            tracing::warn!(
                wanted,
                channel,
                live = ?live,
                note = "wanted domain would not permit the rendezvous channel; not re-asserting",
                "wfb_reg_reassert_skipped_channel_unsafe"
            );
            ReassertOutcome::SkippedChannelUnsafe
        }
        crate::reg_reassert::ReassertDecision::Reassert { from, to } => {
            // Re-issue the global set + verify with the same bounded retry the
            // bring-up gate uses. A self-managed PHY that re-asserts its baked
            // country yields EepromOverride / VerifyTimeout here; we still record
            // the attempt (verified=false) so the action is visible.
            let verified = set_reg_domain(&to).await.is_ok();
            if verified {
                tracing::info!(
                    from = ?from,
                    to = %to,
                    channel,
                    "wfb_reg_domain_reasserted"
                );
            } else {
                tracing::warn!(
                    from = ?from,
                    to = %to,
                    channel,
                    note = "re-assert issued but readback did not confirm (possible phy override)",
                    "wfb_reg_domain_reassert_unconfirmed"
                );
            }
            ReassertOutcome::Reasserted { from, to, verified }
        }
    }
}

/// Return the global regulatory country from `iw reg get`, or None. The first
/// `country XX:` line is the global domain; per-phy self-managed blocks come
/// after it. The injection phy follows the global domain, so that is the one
/// that matters.
async fn active_global_reg_domain() -> Option<String> {
    let out = run_cmd_output("iw", &["reg", "get"]).await.ok()?;
    for line in out.lines() {
        let s = line.trim();
        if let Some(rest) = s.strip_prefix("country ") {
            let cc: String = rest.chars().take(2).collect();
            if cc.len() == 2 {
                return Some(cc.to_ascii_uppercase());
            }
        }
    }
    None
}

/// Restore an interface to managed mode (used on shutdown or profile switch).
pub async fn set_managed_mode(iface: &str) {
    let _ = run_cmd("ip", &["link", "set", iface, "down"]).await;
    let _ = run_cmd("iw", &[iface, "set", "type", "managed"]).await;
    let _ = run_cmd("ip", &["link", "set", iface, "up"]).await;
    let _ = run_cmd("nmcli", &["dev", "set", iface, "managed", "yes"]).await;
}

/// Build the TX-power fallback ramp: the requested dBm first, then 5/7/10 dBm
/// (each only if it is higher than the request and not already present).
/// Mirrors `adapter.py:689-692`. A low request can be rejected by the driver
/// depending on the regulatory domain, so we ramp UP to a value it accepts.
pub fn tx_power_ramp(dbm: i8) -> Vec<i8> {
    let mut ramp = vec![dbm];
    for fallback in [5i8, 7, 10] {
        if fallback > dbm && !ramp.contains(&fallback) {
            ramp.push(fallback);
        }
    }
    ramp
}

/// Apply TX power via `iw dev <iface> set txpower fixed <mBm>` (mBm = dBm×100),
/// ramping up through the fallbacks on driver rejection. Returns the effective
/// dBm that was accepted, or `None` if every step failed.
///
/// **Why this matters:** without it the dongle runs at the driver default
/// (~17-20 dBm), which browns out a host-VBUS-powered RTL adapter — the exact
/// failure `video.wfb.tx_power_dbm = 5` guards against (`adapter.py:674-732`).
/// Readback floor below which a `txpower` value means the interface is pinned at
/// the regulatory "not permitted" floor (reported as `-100.00 dBm`), i.e. a muted
/// PHY that accepts injected frames but radiates nothing. Any genuine setting is
/// >= 0 dBm, so a readback at or below this is unambiguously the muted floor.
pub const MUTED_TX_POWER_DBM: f32 = -10.0;

/// Read the live TX power (dBm) from `iw dev <iface> info`, or `None` when it
/// cannot be read or parsed. Used to verify a `set txpower` actually took.
pub async fn read_tx_power(iface: &str) -> Option<f32> {
    let out = run_cmd_output("iw", &["dev", iface, "info"]).await.ok()?;
    for line in out.lines() {
        if let Some(rest) = line.trim().strip_prefix("txpower ") {
            return rest.split_whitespace().next()?.parse::<f32>().ok();
        }
    }
    None
}

/// Apply TX power with the region-mode muted-readback policy (region: abort on a
/// muted PHY by returning None). Thin wrapper over [`set_tx_power_modal`] so
/// existing region-mode callers stay unchanged.
pub async fn set_tx_power(iface: &str, dbm: i8) -> Option<i8> {
    set_tx_power_modal(iface, dbm, false).await
}

/// Apply TX power, threading the unrestricted operating posture.
///
/// `unrestricted` controls only the muted-readback handling. A muted readback
/// (<= the not-permitted floor) is ALWAYS logged + surfaces as `rf_unverified`
/// downstream (the received-side dual-check stays armed in both postures). The
/// difference is the return value:
/// - region mode (`unrestricted == false`): return `None` — the muted PHY is a
///   dead radio under an enforced region, so the caller does not promote it.
/// - unrestricted mode (`unrestricted == true`): return `Some(candidate)` — under
///   the operator-responsible posture we surface the muted state rather than
///   aborting bring-up, because a self-managed PHY can read back muted on the
///   not-permitted floor before the driver-region layer settles, yet still need
///   the rest of the bring-up to proceed. The honest `rf_unverified` signal is
///   what tells the operator the radio is not yet radiating.
pub async fn set_tx_power_modal(iface: &str, dbm: i8, unrestricted: bool) -> Option<i8> {
    for candidate in tx_power_ramp(dbm) {
        let mbm = (candidate as i32) * 100;
        if run_cmd(
            "iw",
            &["dev", iface, "set", "txpower", "fixed", &mbm.to_string()],
        )
        .await
        .is_ok()
        {
            // A zero exit from `iw set txpower` is necessary but NOT sufficient:
            // a regulatory/PHY perturbation (e.g. an `iw reg set` churn re-entering
            // monitor) can leave the interface pinned at the "-100 dBm" not-permitted
            // floor while the set command still returns success. That muted PHY
            // injects frames into the TX ring (the counter advances) but radiates
            // nothing. Read the value back: in region mode this is a fatal "dead
            // radio" verdict; under the unrestricted posture we log + surface it
            // (rf_unverified) but let bring-up continue.
            if let Some(live) = read_tx_power(iface).await {
                if live <= MUTED_TX_POWER_DBM {
                    tracing::error!(
                        iface,
                        requested = dbm,
                        applied = candidate,
                        readback_dbm = live,
                        unrestricted,
                        "wfb_txpower_muted_readback"
                    );
                    if !unrestricted {
                        return None;
                    }
                    // Unrestricted: surface, do not abort. The muted readback is
                    // still honestly reported; return the candidate so the rest of
                    // bring-up proceeds and the rf_unverified detector carries the
                    // truth that no RF is leaving the antenna.
                    return Some(candidate);
                }
            }
            if candidate != dbm {
                tracing::warn!(
                    iface,
                    requested = dbm,
                    applied = candidate,
                    "wfb_txpower_fallback"
                );
            } else {
                tracing::info!(iface, dbm = candidate, "wfb_txpower_applied");
            }
            return Some(candidate);
        }
    }
    tracing::error!(iface, requested = dbm, "wfb_txpower_all_steps_rejected");
    None
}

async fn run_cmd(cmd: &str, args: &[&str]) -> Result<(), ()> {
    let status = tokio::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map_err(|_| ())?;
    if status.success() {
        Ok(())
    } else {
        Err(())
    }
}

async fn run_cmd_output(cmd: &str, args: &[&str]) -> Result<String, ()> {
    let out = tokio::process::Command::new(cmd)
        .args(args)
        .output()
        .await
        .map_err(|_| ())?;
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
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
    fn tx_power_ramp_low_request_adds_fallbacks() {
        // A 5 dBm request: 5 is not < itself, 7 and 10 are higher → ramp up.
        assert_eq!(tx_power_ramp(5), vec![5, 7, 10]);
    }

    #[test]
    fn tx_power_ramp_high_request_has_no_fallbacks() {
        // A 15 dBm request: no fallback exceeds it.
        assert_eq!(tx_power_ramp(15), vec![15]);
    }

    #[test]
    fn tx_power_ramp_mid_request() {
        // 7 dBm: only 10 is higher.
        assert_eq!(tx_power_ramp(7), vec![7, 10]);
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
    fn parse_default_route_extracts_dev_iface() {
        let line = "default via 192.168.200.1 dev end1 proto dhcp metric 100";
        assert_eq!(parse_default_route_iface(line).as_deref(), Some("end1"));
        // A `dev`-first ordering (no `via`) still resolves.
        assert_eq!(
            parse_default_route_iface("default dev wwan0 scope link").as_deref(),
            Some("wwan0")
        );
    }

    #[test]
    fn parse_default_route_no_default_is_none() {
        // No default-route line in the output → None.
        assert!(parse_default_route_iface("10.0.0.0/24 dev eth0 scope link").is_none());
        assert!(parse_default_route_iface("").is_none());
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
    fn reg_domain_format_accepts_valid_rejects_malformed() {
        // Two uppercase letters or digits → accepted.
        assert!(is_valid_reg_domain("IN"));
        assert!(is_valid_reg_domain("US"));
        assert!(is_valid_reg_domain("00")); // world domain
                                            // Anything else → rejected before it reaches `iw reg set`.
        assert!(!is_valid_reg_domain("in")); // lowercase
        assert!(!is_valid_reg_domain("USA")); // too long
        assert!(!is_valid_reg_domain("I")); // too short
        assert!(!is_valid_reg_domain("")); // empty
        assert!(!is_valid_reg_domain("I N")); // whitespace / wrong length
        assert!(!is_valid_reg_domain("U;")); // injected punctuation
    }

    #[test]
    fn parse_dfs_channels_keeps_radar_and_no_ir_only() {
        let text = "\
Band 2:
	Frequencies:
		* 5180 MHz [36] (disabled)
		* 5200 MHz [40] (20.0 dBm)
		* 5260 MHz [52] (no IR, radar detection)
		* 5300 MHz [60] (radar detection)
		* 5745 MHz [149] (30.0 dBm)
";
        let got = parse_dfs_channels(text);
        // 36 is disabled (not DFS), 40/149 are usable (not DFS); 52/60 are DFS.
        let want: std::collections::BTreeSet<u8> = [52, 60].into_iter().collect();
        assert_eq!(got, want);
    }

    #[test]
    fn assert_reg_ready_passes_when_channel_enabled_and_non_dfs() {
        let enabled: std::collections::BTreeSet<u8> = [36, 40, 149, 153].into_iter().collect();
        let dfs: std::collections::BTreeSet<u8> = [52, 60].into_iter().collect();
        assert!(assert_reg_ready(149, &enabled, &dfs, false).is_ok());
    }

    #[test]
    fn assert_reg_ready_rejects_channel_not_enabled() {
        let enabled: std::collections::BTreeSet<u8> = [36, 40, 149].into_iter().collect();
        let dfs = std::collections::BTreeSet::new();
        assert_eq!(
            assert_reg_ready(165, &enabled, &dfs, false),
            Err(RegError::ChannelNotEnabled { channel: 165 })
        );
    }

    #[test]
    fn assert_reg_ready_rejects_dfs_home_unless_allowed() {
        let enabled: std::collections::BTreeSet<u8> = [52, 149].into_iter().collect();
        let dfs: std::collections::BTreeSet<u8> = [52].into_iter().collect();
        // DFS home refused by default.
        assert_eq!(
            assert_reg_ready(52, &enabled, &dfs, false),
            Err(RegError::ChannelIsDfs { channel: 52 })
        );
        // Opt-in clears it (the channel is still in the enabled set).
        assert!(assert_reg_ready(52, &enabled, &dfs, true).is_ok());
    }

    #[test]
    fn assert_reg_ready_passes_when_enabled_set_unknown() {
        // An empty enabled set means "could not read the wiphy list" — the gate
        // must not wedge a board whose channel list is unreadable.
        let empty = std::collections::BTreeSet::new();
        let dfs = std::collections::BTreeSet::new();
        assert!(assert_reg_ready(149, &empty, &dfs, false).is_ok());
    }

    #[test]
    fn self_managed_phy_with_conflicting_country_is_detected() {
        // The live override shape: global says US, a self-managed phy bakes BO.
        let text = "\
global
country US: DFS-FCC
	(5170 - 5250 @ 80), (N/A, 17), (N/A)
phy#3 (self-managed)
country BO: DFS-UNSET
	(5170 - 5250 @ 80), (N/A, 20), (N/A)
";
        assert_eq!(
            parse_conflicting_self_managed_phy(text, "US"),
            Some(("phy3".to_string(), "BO".to_string()))
        );
    }

    #[test]
    fn self_managed_phy_matching_global_is_not_a_conflict() {
        // A self-managed phy that already carries the wanted country is fine.
        let text = "\
global
country US: DFS-FCC
phy#0 (self-managed)
country US: DFS-FCC
";
        assert_eq!(parse_conflicting_self_managed_phy(text, "US"), None);
    }

    #[test]
    fn non_self_managed_country_block_is_not_an_override() {
        // The plain global block (no self-managed phy) is never an override, even
        // when it differs from the wanted domain — that is the retry/timeout path,
        // not the unrecoverable EEPROM case.
        let text = "\
global
country BO: DFS-UNSET
	(5170 - 5250 @ 80), (N/A, 20), (N/A)
";
        assert_eq!(parse_conflicting_self_managed_phy(text, "US"), None);
    }

    #[test]
    fn self_managed_spelling_variants_both_parse() {
        // `iw` has emitted both "self managed" and "self-managed".
        let spaced = "phy3 (self managed)\ncountry BO: DFS-UNSET\n";
        assert_eq!(
            parse_conflicting_self_managed_phy(spaced, "US"),
            Some(("phy3".to_string(), "BO".to_string()))
        );
    }

    #[test]
    fn reg_error_reason_codes_are_stable_and_bland() {
        assert_eq!(RegError::CommandFailed.reason_code(), "command_failed");
        assert_eq!(RegError::InvalidFormat.reason_code(), "invalid_format");
        assert_eq!(
            RegError::VerifyTimeout {
                want: "US".into(),
                got: Some("BO".into())
            }
            .reason_code(),
            "verify_timeout"
        );
        assert_eq!(
            RegError::EepromOverride {
                want: "US".into(),
                got: "BO".into()
            }
            .reason_code(),
            "phy_override"
        );
        assert_eq!(
            RegError::ChannelNotEnabled { channel: 165 }.reason_code(),
            "channel_not_enabled"
        );
        assert_eq!(
            RegError::ChannelIsDfs { channel: 52 }.reason_code(),
            "channel_dfs"
        );
    }

    #[test]
    fn reg_is_verified_matches_wanted_domain_case_insensitively() {
        // Live domain equals the wanted domain → verified.
        assert!(reg_is_verified(Some("US"), "US"));
        // Case does not matter (iw can emit either).
        assert!(reg_is_verified(Some("us"), "US"));
        assert!(reg_is_verified(Some("US"), "us"));
        // A different live domain (the forbidden-band case) → not verified.
        assert!(!reg_is_verified(Some("BO"), "US"));
        // Unknown live domain (iw unreadable) → not verified.
        assert!(!reg_is_verified(None, "US"));
        // Empty wanted domain → nothing to match, never verified.
        assert!(!reg_is_verified(Some("US"), ""));
        assert!(!reg_is_verified(None, ""));
    }

    #[test]
    fn parse_mode_reads_type_line() {
        let info = "Interface wlan1\n\tifindex 5\n\twdev 0x1\n\ttype monitor\n\twiphy 0\n";
        assert_eq!(parse_interface_mode(info).as_deref(), Some("monitor"));
        let managed = "Interface wlan1\n\ttype managed\n";
        assert_eq!(parse_interface_mode(managed).as_deref(), Some("managed"));
    }

    #[test]
    fn parse_mode_missing_type_is_none() {
        assert!(parse_interface_mode("Interface wlan1\n\tifindex 5\n").is_none());
        assert!(parse_interface_mode("").is_none());
    }

    #[test]
    fn parse_wiphy_extracts_phy_name() {
        let info = "Interface wlan1\n\ttype monitor\n\twiphy 0\n";
        assert_eq!(parse_wiphy(info).as_deref(), Some("phy0"));
        let info2 = "Interface wlan1\n\twiphy 3\n";
        assert_eq!(parse_wiphy(info2).as_deref(), Some("phy3"));
    }

    #[test]
    fn parse_wiphy_missing_is_none() {
        assert!(parse_wiphy("Interface wlan1\n\ttype monitor\n").is_none());
    }

    #[test]
    fn parse_enabled_channels_keeps_usable_skips_disabled_and_dfs() {
        let text = "\
Band 2:
	Frequencies:
		* 5180 MHz [36] (disabled)
		* 5200 MHz [40] (20.0 dBm)
		* 5260 MHz [52] (no IR, radar detection)
		* 5300 MHz [60] (radar detection)
		* 5745 MHz [149] (30.0 dBm)
		* 5765 MHz [153] (30.0 dBm)
";
        let got = parse_enabled_channels(text);
        let want: std::collections::BTreeSet<u8> = [40, 149, 153].into_iter().collect();
        assert_eq!(got, want);
    }

    #[test]
    fn parse_enabled_channels_empty_input_is_empty() {
        assert!(parse_enabled_channels("").is_empty());
        // A line with no bracket token contributes nothing.
        assert!(parse_enabled_channels("Band 2:\n\tFrequencies:\n").is_empty());
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
