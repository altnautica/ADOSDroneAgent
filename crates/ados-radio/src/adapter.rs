//! RTL8812EU adapter selection and monitor-mode setup.
//!
//! Mirrors `services/wfb/adapter.py`: scans network interfaces, identifies
//! RTL injection radios by USB VID/PID or driver name, denies AIC8800 and
//! brcmfmac management-wifi adapters, validates monitor mode with a 4× retry.
//!
//! All OS calls (iw, nmcli, ip) are Linux-only. On non-Linux hosts this
//! module compiles but `select_interface` always returns None.

/// Well-known RTL chipset (VID, PID) → label mapping.
/// PID 0xA81A is ambiguous: driver name disambiguates EU vs AU.
#[cfg(any(target_os = "linux", test))]
const RTL_VID_PIDS: &[([u8; 2], [u8; 2], &str)] = &[
    ([0x0B, 0xDA], [0x88, 0x12], "RTL8812EU"),
    ([0x0B, 0xDA], [0x88, 0x11], "RTL8811AU"),
    ([0x0B, 0xDA], [0xA8, 0x1A], "RTL8812EU (a81a)"), // driver confirms EU vs AU
];

/// VID deny-set: AIC8800 management-only adapters must never be used.
#[cfg(any(target_os = "linux", test))]
const DENY_VID: &[u16] = &[0xA69C];

/// Driver prefix deny-set.
#[cfg(any(target_os = "linux", test))]
const DENY_DRIVER_PREFIXES: &[&str] = &["aic8800", "brcmfmac"];

/// An adapter candidate discovered by the scan.
#[derive(Debug, Clone)]
pub struct Adapter {
    pub ifname: String,
    pub chipset: String,
    pub driver: String,
    pub injection_rank: u8, // lower = preferred (EU=0, AU=1, other=2)
}

/// The result returned to the radio manager.
#[derive(Debug, Clone)]
pub struct SelectedAdapter {
    pub ifname: String,
    pub chipset: String,
    pub injection_ok: bool,
}

/// Return the best available RTL injection adapter, or `None` if none found.
/// Sets the adapter into monitor mode and verifies the readback before returning.
#[cfg(target_os = "linux")]
pub async fn select_interface(override_iface: &str) -> Option<SelectedAdapter> {
    if !override_iface.is_empty() {
        // Operator-specified interface: skip discovery, still validate.
        let chipset = chipset_for_iface(override_iface);
        let ok = set_monitor_mode_verified(override_iface, 4).await;
        return Some(SelectedAdapter {
            ifname: override_iface.to_string(),
            chipset,
            injection_ok: ok,
        });
    }
    let mut candidates = scan_adapters().await;
    // Sort: EU rank 0 first.
    candidates.sort_by_key(|a| a.injection_rank);
    for adapter in candidates {
        tracing::info!(iface = %adapter.ifname, chipset = %adapter.chipset, "adapter_candidate");
        let ok = set_monitor_mode_verified(&adapter.ifname, 4).await;
        if ok {
            return Some(SelectedAdapter {
                ifname: adapter.ifname,
                chipset: adapter.chipset,
                injection_ok: true,
            });
        }
        tracing::warn!(iface = %adapter.ifname, "adapter_injection_failed");
    }
    None
}

#[cfg(not(target_os = "linux"))]
pub async fn select_interface(_override_iface: &str) -> Option<SelectedAdapter> {
    None
}

/// Scan all network interfaces and return RTL-compatible candidates that pass
/// the deny filter.
#[cfg(target_os = "linux")]
async fn scan_adapters() -> Vec<Adapter> {
    let mut out = Vec::new();
    let Ok(ifaces) = tokio::fs::read_dir("/sys/class/net").await else {
        return out;
    };
    // The iface carrying the default route is the operator's control path; it
    // must never be claimed as an injection radio.
    let control_iface = control_interface().await;
    let mut ifaces = ifaces;
    while let Ok(Some(entry)) = ifaces.next_entry().await {
        let ifname = entry.file_name().to_string_lossy().to_string();
        // Only USB-backed (wireless) — skip lo, eth*, etc.
        if !is_wireless(&ifname).await {
            continue;
        }
        // Never claim the control interface, even if it is wireless.
        if control_iface.as_deref() == Some(ifname.as_str()) {
            tracing::info!(iface = %ifname, "adapter_excluded_control_iface");
            continue;
        }
        let driver = driver_name(&ifname).await;
        let vid_pid = usb_vid_pid(&ifname).await;
        // Apply deny filters first.
        if is_denied(&driver, vid_pid) {
            tracing::debug!(iface = %ifname, driver = %driver, "adapter_denied");
            continue;
        }
        let (chipset, rank) = classify(&driver, vid_pid);
        if rank < 3 {
            // Known RTL family.
            out.push(Adapter {
                ifname,
                chipset,
                driver,
                injection_rank: rank,
            });
        }
    }
    out
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

#[cfg(any(target_os = "linux", test))]
fn classify(driver: &str, vid_pid: Option<(u16, u16)>) -> (String, u8) {
    // Driver-name takes precedence for PID-ambiguous adapters.
    if driver.contains("rtl88x2eu") || driver.contains("8812eu") {
        return ("RTL8812EU".to_string(), 0);
    }
    if driver.contains("rtl8812au") || driver.contains("88xxau") {
        return ("RTL8812AU".to_string(), 1);
    }
    if let Some((vid, pid)) = vid_pid {
        let vid_bytes = vid.to_be_bytes();
        let pid_bytes = pid.to_be_bytes();
        for (v, p, label) in RTL_VID_PIDS {
            if vid_bytes == *v && pid_bytes == *p {
                let rank = if label.contains("EU") { 0 } else { 1 };
                return (label.to_string(), rank);
            }
        }
    }
    (format!("unknown (driver={})", driver), 3)
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
    match run_cmd_output("iw", &[iface, "info"]).await {
        Ok(out) => out.contains("type monitor"),
        Err(_) => false,
    }
}

/// Apply the regulatory domain via `iw reg set <domain>` (best-effort). Without
/// it the driver may reject channels the domain doesn't allow (`-22`). Mirrors
/// `adapter.py:643-671`. No-op when `domain` is empty.
pub async fn set_reg_domain(domain: &str) {
    if domain.is_empty() {
        return;
    }
    match run_cmd("iw", &["reg", "set", domain]).await {
        Ok(()) => tracing::info!(domain, "wfb_reg_domain_applied"),
        Err(()) => tracing::warn!(domain, "wfb_reg_domain_failed"),
    }
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
}
