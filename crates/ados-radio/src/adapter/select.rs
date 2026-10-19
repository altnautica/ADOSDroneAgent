//! Injection candidate ranking and monitor-mode verification of every WFB
//! adapter on the node.
//!
//! [`detect_all`] scans the radios, keeps the WFB-compatible monitor-capable
//! ones, ranks them RTL-family-first (an operator pin moves to the front only
//! when it passes the same gate), and proves each one by setting and reading
//! back monitor mode. Every candidate comes back with its verdict, so a node
//! with several adapters can hand all of them to the receive plane and the
//! role resolver in `roles` decides what each one does.

use super::detect::WifiAdapterInfo;
#[cfg(any(target_os = "linux", test))]
use super::detect::{driver_is_wfb_compatible, wfb_compatible_label};

/// One WFB injection candidate and its monitor-mode verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedAdapter {
    pub ifname: String,
    /// The adapter's MAC address, `None` when unreadable.
    pub mac: Option<String>,
    /// The USB device path (`1-1.2`), `None` when not USB-backed.
    pub usb_path: Option<String>,
    pub driver: String,
    pub chipset: String,
    /// Monitor mode was set and read back on this adapter.
    pub verified: bool,
    /// Enumerated USB link speed (Mbps), `None` when not USB-backed or
    /// unreadable.
    pub usb_speed_mbps: Option<u32>,
    /// True when the adapter enumerated below high-speed USB. Such an adapter
    /// can pass monitor-mode setup and advance its counters yet carry no usable
    /// RF, so it is surfaced rather than reported healthy.
    pub usb_degraded: bool,
}

/// The outcome of one detection pass.
#[derive(Debug, Clone, Default)]
pub struct DetectionOutcome {
    /// Every detected WiFi adapter, compatible or not, written verbatim to the
    /// adapters sidecar so the panel can render the full scan verdict.
    pub scanned: Vec<WifiAdapterInfo>,
    /// The injection candidates in ranked order, each with its verdict.
    pub candidates: Vec<DetectedAdapter>,
}

impl DetectionOutcome {
    /// Total adapters detected.
    pub fn total(&self) -> usize {
        self.scanned.len()
    }

    /// Adapters that are WFB-compatible AND advertise monitor mode.
    pub fn compatible_monitor(&self) -> usize {
        self.scanned
            .iter()
            .filter(|a| a.is_wfb_compatible && a.supports_monitor)
            .count()
    }

    /// The candidates whose monitor mode verified, in ranked order.
    pub fn verified(&self) -> Vec<DetectedAdapter> {
        self.candidates
            .iter()
            .filter(|a| a.verified)
            .cloned()
            .collect()
    }
}

/// Detect every WFB adapter and verify monitor mode on each candidate.
///
/// An operator-pinned `override_iface` is moved to the front of the candidates
/// only when it names a detected adapter that passes the gate (not management
/// WiFi, WFB compatible, monitor capable). Interface names are not identities,
/// so a pin that now names the onboard WiFi or a missing device is logged and
/// ignored rather than put into monitor mode.
#[cfg(target_os = "linux")]
pub async fn detect_all(override_iface: &str) -> DetectionOutcome {
    let scanned = super::detect::detect_wfb_adapters().await;
    // Bench builds only: a pin on a `dummy` link joins the scan as an adapter
    // and then faces the same candidate gate and monitor-mode readback.
    #[cfg(feature = "bench-adapters")]
    let scanned = super::bench::with_pinned_dummy(scanned, override_iface).await;

    let mut candidates = Vec::new();
    for adapter in injection_candidates(&scanned, override_iface) {
        let ifname = adapter.interface_name.clone();
        tracing::info!(
            interface = %ifname,
            chipset = %adapter.chipset,
            rank = injection_rank(adapter),
            "wfb_adapter_candidate"
        );
        let verified = super::monitor::set_monitor_mode_verified(&ifname, 4).await;
        let usb_degraded = super::detect::usb_speed_degraded(adapter.usb_speed_mbps);
        if !verified {
            tracing::warn!(interface = %ifname, chipset = %adapter.chipset, "wfb_adapter_monitor_rejected");
        } else if usb_degraded {
            tracing::warn!(
                interface = %ifname,
                chipset = %adapter.chipset,
                usb_speed_mbps = ?adapter.usb_speed_mbps,
                "wfb_adapter_usb_degraded: adapter on a slow USB link (needs 480 Mbps); RF may not transmit"
            );
        } else {
            tracing::info!(interface = %ifname, chipset = %adapter.chipset, "wfb_adapter_verified");
        }
        candidates.push(DetectedAdapter {
            mac: super::detect::mac_address(&ifname).await,
            usb_path: super::detect::usb_path(&ifname).await,
            driver: adapter.driver.clone(),
            chipset: adapter.chipset.clone(),
            verified,
            usb_speed_mbps: adapter.usb_speed_mbps,
            usb_degraded,
            ifname,
        });
    }
    DetectionOutcome {
        scanned,
        candidates,
    }
}

#[cfg(not(target_os = "linux"))]
pub async fn detect_all(_override_iface: &str) -> DetectionOutcome {
    DetectionOutcome::default()
}

/// Rank an adapter so the validated RTL injection radios float to the front
/// (lower is better): RTL8812EU silicon first, RTL8812AU rebadges next, any
/// other passing chip last. Makes selection independent of USB bus order so a
/// management WiFi enumerated first can never win by accident.
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

/// The adapters eligible for injection, in the order selection tries them:
/// compatible + monitor-capable only (the deny gate already cleared management
/// WiFi's compatibility flag), ranked RTL-family-first, with an operator pin
/// moved to the front only when it names one of those. A pin that names
/// anything else is logged and ignored. Pure so the "a pin never bypasses the
/// gate" rule is testable off a real SBC.
#[cfg(any(target_os = "linux", test))]
fn injection_candidates<'a>(
    adapters: &'a [WifiAdapterInfo],
    override_iface: &str,
) -> Vec<&'a WifiAdapterInfo> {
    let mut candidates: Vec<&WifiAdapterInfo> = adapters
        .iter()
        .filter(|a| a.is_wfb_compatible && a.supports_monitor)
        .collect();
    // Name order first so equal ranks come back in a stable order regardless
    // of the sysfs directory walk; the stable rank sort then keeps it.
    candidates.sort_by(|a, b| a.interface_name.cmp(&b.interface_name));
    candidates.sort_by_key(|a| injection_rank(a));
    if !override_iface.is_empty() {
        match candidates
            .iter()
            .position(|a| a.interface_name == override_iface)
        {
            Some(idx) => {
                let pinned = candidates.remove(idx);
                candidates.insert(0, pinned);
            }
            None => tracing::warn!(
                interface = %override_iface,
                "wfb_interface_override_ignored: not a detected WFB-compatible monitor-capable adapter"
            ),
        }
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::super::detect::build_adapter_info;
    use super::*;

    #[test]
    fn equal_ranks_come_back_in_name_order() {
        let adapters = vec![
            monitor_adapter("wlan3", "rtl88x2eu"),
            monitor_adapter("wlan1", "rtl88x2eu"),
            monitor_adapter("wlan2", "rtl8812au"),
            monitor_adapter("wlan0", "rtl88x2eu"),
        ];
        let names: Vec<&str> = injection_candidates(&adapters, "")
            .into_iter()
            .map(|a| a.interface_name.as_str())
            .collect();
        assert_eq!(names, vec!["wlan0", "wlan1", "wlan3", "wlan2"]);
    }

    #[test]
    fn verified_keeps_only_proven_candidates_in_order() {
        let cand = |name: &str, verified: bool| DetectedAdapter {
            ifname: name.into(),
            mac: None,
            usb_path: None,
            driver: "rtl88x2eu".into(),
            chipset: "RTL8812EU".into(),
            verified,
            usb_speed_mbps: Some(480),
            usb_degraded: false,
        };
        let outcome = DetectionOutcome {
            scanned: Vec::new(),
            candidates: vec![
                cand("wlan1", true),
                cand("wlan2", false),
                cand("wlan3", true),
            ],
        };
        let names: Vec<String> = outcome.verified().into_iter().map(|a| a.ifname).collect();
        assert_eq!(names, vec!["wlan1", "wlan3"]);
    }

    fn monitor_adapter(name: &str, driver: &str) -> WifiAdapterInfo {
        build_adapter_info(
            name.to_string(),
            driver.to_string(),
            None,
            "phy0".to_string(),
            vec!["managed".to_string(), "monitor".to_string()],
            true,
            Some("managed".to_string()),
        )
    }

    /// An interface pin can reorder the gated candidates but never add to them:
    /// a name that now points at the onboard management WiFi (names swap between
    /// reboots) is ignored, and selection falls back to the RTL adapter.
    #[test]
    fn an_interface_pin_never_bypasses_the_injection_gate() {
        let adapters = vec![
            monitor_adapter("wlan0", "brcmfmac"),
            monitor_adapter("wlan1", "rtl88x2eu"),
            monitor_adapter("wlan2", "rtl8812au"),
        ];
        let names = |c: Vec<&WifiAdapterInfo>| -> Vec<String> {
            c.into_iter().map(|a| a.interface_name.clone()).collect()
        };
        // A pin on the management WiFi never makes it a candidate.
        assert_eq!(
            names(injection_candidates(&adapters, "wlan0")),
            vec!["wlan1", "wlan2"]
        );
        // A pin on a missing device is ignored the same way.
        assert_eq!(
            names(injection_candidates(&adapters, "wlan9")),
            vec!["wlan1", "wlan2"]
        );
        // A pin on a gated adapter moves it to the front.
        assert_eq!(
            names(injection_candidates(&adapters, "wlan2")),
            vec!["wlan2", "wlan1"]
        );
    }

    /// Bench builds only: a pin on a `dummy` link becomes the first candidate,
    /// and the gate still holds for every other pin. Release builds never
    /// compile this path; there `an_interface_pin_never_bypasses_the_injection_gate`
    /// is the whole rule.
    #[cfg(feature = "bench-adapters")]
    #[test]
    fn a_bench_build_selects_a_pinned_dummy_link() {
        let names = |c: Vec<&WifiAdapterInfo>| -> Vec<String> {
            c.into_iter().map(|a| a.interface_name.clone()).collect()
        };
        let mut adapters = vec![
            monitor_adapter("wlan0", "brcmfmac"),
            monitor_adapter("wlan2", "rtl8812au"),
        ];
        let dummy = super::super::bench::pinned_dummy_adapter(&adapters, "wlan1", Some("dummy"))
            .expect("a pinned dummy link is a bench adapter");
        adapters.push(dummy);
        assert_eq!(
            names(injection_candidates(&adapters, "wlan1")),
            vec!["wlan1", "wlan2"]
        );
        // The management WiFi pin is still refused in a bench build.
        assert!(
            super::super::bench::pinned_dummy_adapter(&adapters, "wlan0", Some("dummy")).is_none()
        );
        assert_eq!(
            names(injection_candidates(&adapters, "wlan0")),
            vec!["wlan2", "wlan1"]
        );
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
}
