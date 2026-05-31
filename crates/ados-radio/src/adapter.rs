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
        out.push(build_adapter_info(
            name,
            driver,
            vid_pid,
            phy,
            capabilities,
            supports_monitor,
            current_mode,
        ));
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
        return SelectionOutcome {
            adapters,
            selected: Some(SelectedAdapter {
                ifname: override_iface.to_string(),
                chipset,
                injection_ok: ok,
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
            tracing::info!(
                interface = %adapter.interface_name,
                chipset = %adapter.chipset,
                "wfb_adapter_selected"
            );
            selected = Some(SelectedAdapter {
                ifname: adapter.interface_name.clone(),
                chipset: adapter.chipset.clone(),
                injection_ok: true,
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

#[cfg(target_os = "linux")]
async fn usb_vid_pid(iface: &str) -> Option<(u16, u16)> {
    // /sys/class/net/<if>/device/idVendor + idProduct (USB device).
    let base = format!("/sys/class/net/{}/device", iface);
    let vid_raw = tokio::fs::read_to_string(format!("{}/idVendor", base))
        .await
        .ok()?;
    let pid_raw = tokio::fs::read_to_string(format!("{}/idProduct", base))
        .await
        .ok()?;
    let vid = u16::from_str_radix(vid_raw.trim(), 16).ok()?;
    let pid = u16::from_str_radix(pid_raw.trim(), 16).ok()?;
    Some((vid, pid))
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
    // Set monitor — primary form, fallback to legacy form on error.
    let set_ok = if run_cmd("iw", &[iface, "set", "type", "monitor"])
        .await
        .is_ok()
    {
        true
    } else {
        run_cmd("iw", &[iface, "set", "monitor", "none"])
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

/// Apply the regulatory domain via `iw reg set <domain>` and verify it took.
///
/// Must run BEFORE the interface is brought up in monitor mode: the kernel
/// maps the permitted channel set and the per-channel TX-power ceiling when
/// the driver initialises, so a domain set afterwards is too late and leaves
/// the home channel (149, U-NII-3 / 5745 MHz) capped to the startup domain's
/// limits (the -100 dBm "not permitted" sentinel, zero injected frames).
/// `iw reg set` is asynchronous, so this polls `iw reg get` until the global
/// country reflects the request (~2 s ceiling). Best-effort: a failure is
/// logged and the link continues. No-op when `domain` is empty.
pub async fn set_reg_domain(domain: &str) {
    if domain.is_empty() {
        return;
    }
    if run_cmd("iw", &["reg", "set", domain]).await.is_err() {
        tracing::warn!(domain, "wfb_reg_domain_failed");
        return;
    }
    let want = domain.to_ascii_uppercase();
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(2);
    loop {
        if active_global_reg_domain().await.as_deref() == Some(want.as_str()) {
            tracing::info!(domain, applied = true, "wfb_reg_domain_applied");
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::info!(domain, applied = false, "wfb_reg_domain_applied");
            return;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
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
pub async fn set_tx_power(iface: &str, dbm: i8) -> Option<i8> {
    for candidate in tx_power_ramp(dbm) {
        let mbm = (candidate as i32) * 100;
        if run_cmd(
            "iw",
            &["dev", iface, "set", "txpower", "fixed", &mbm.to_string()],
        )
        .await
        .is_ok()
        {
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
        assert_eq!(v["is_wfb_compatible"], true);
        assert_eq!(v["capabilities"], serde_json::json!(["managed", "monitor"]));
        // The contract carries exactly the ten Python dataclass keys.
        assert_eq!(v.as_object().unwrap().len(), 10);
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
