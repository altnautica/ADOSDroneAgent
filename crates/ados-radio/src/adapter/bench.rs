//! Bench-only adapter seam, compiled only with the `bench-adapters` cargo
//! feature.
//!
//! A hardware-free bench runs the real agent in VMs with no radio. It stands a
//! `dummy` network link in for the flight radio and pins `video.wfb.interface`
//! to it. The injection gate in `detect` would refuse that pin, because a dummy
//! link is not 802.11 and is never scanned as an adapter. In a bench build
//! only, a pin that names a `dummy` link is added to the scan as a WFB
//! compatible, monitor-capable adapter, so it goes through the same candidate
//! ranking and monitor-mode readback as a real radio.
//!
//! The feature is off in every release and CI publish build. There, this module
//! does not exist and the gate is absolute: no environment variable or file can
//! make a dummy link selectable.

#[cfg(any(target_os = "linux", test))]
use super::detect::WifiAdapterInfo;

/// The link kind the bench uses for its stand-in radio.
#[cfg(any(target_os = "linux", test))]
const BENCH_LINK_KIND: &str = "dummy";

/// The chipset label a bench adapter reports, so a sidecar or log line read
/// later cannot be mistaken for a real radio.
#[cfg(any(target_os = "linux", test))]
const BENCH_CHIPSET: &str = "bench-dummy";

/// The rtnetlink link kind (`linkinfo.info_kind`) from `ip -d -j link show dev
/// <iface>` output, or `None` when the output does not carry one (a physical
/// NIC has no kind).
#[cfg(any(target_os = "linux", test))]
fn link_kind_from_ip_json(text: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    v.get(0)?
        .get("linkinfo")?
        .get("info_kind")?
        .as_str()
        .map(str::to_string)
}

/// The adapter a bench build adds for an operator pin, or `None`.
///
/// Only a non-empty pin whose link kind is `dummy` qualifies, and only when the
/// scan did not already report an adapter under that name. Pure so both the
/// acceptance and every refusal are testable without a link.
#[cfg(any(target_os = "linux", test))]
pub(super) fn pinned_dummy_adapter(
    adapters: &[WifiAdapterInfo],
    override_iface: &str,
    link_kind: Option<&str>,
) -> Option<WifiAdapterInfo> {
    if override_iface.is_empty()
        || link_kind != Some(BENCH_LINK_KIND)
        || adapters.iter().any(|a| a.interface_name == override_iface)
    {
        return None;
    }
    Some(WifiAdapterInfo {
        interface_name: override_iface.to_string(),
        driver: BENCH_LINK_KIND.to_string(),
        chipset: BENCH_CHIPSET.to_string(),
        supports_monitor: true,
        current_mode: None,
        phy: String::new(),
        usb_vid: None,
        usb_pid: None,
        usb_speed_mbps: None,
        is_wfb_compatible: true,
        capabilities: vec!["managed".to_string(), "monitor".to_string()],
    })
}

/// The scan plus the bench adapter for a pinned `dummy` link, when there is one.
#[cfg(target_os = "linux")]
pub(super) async fn with_pinned_dummy(
    mut adapters: Vec<WifiAdapterInfo>,
    override_iface: &str,
) -> Vec<WifiAdapterInfo> {
    if override_iface.is_empty() {
        return adapters;
    }
    let kind = super::run_cmd_output("ip", &["-d", "-j", "link", "show", "dev", override_iface])
        .await
        .ok()
        .and_then(|out| link_kind_from_ip_json(&out));
    if let Some(dummy) = pinned_dummy_adapter(&adapters, override_iface, kind.as_deref()) {
        tracing::warn!(
            interface = %override_iface,
            "wfb_bench_dummy_adapter: bench build, a pinned dummy link stands in for the radio"
        );
        adapters.push(dummy);
    }
    adapters
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_link_kind_from_ip_json() {
        let dummy = r#"[{"ifindex":3,"ifname":"wlan1","linkinfo":{"info_kind":"dummy"}}]"#;
        assert_eq!(link_kind_from_ip_json(dummy).as_deref(), Some("dummy"));
        // A physical NIC carries no link kind at all.
        let nic = r#"[{"ifindex":2,"ifname":"eth0","link_type":"ether"}]"#;
        assert_eq!(link_kind_from_ip_json(nic), None);
        assert_eq!(link_kind_from_ip_json(""), None);
        assert_eq!(link_kind_from_ip_json("[]"), None);
    }

    #[test]
    fn only_a_pinned_dummy_link_becomes_a_bench_adapter() {
        let none: Vec<WifiAdapterInfo> = Vec::new();
        let added = pinned_dummy_adapter(&none, "wlan1", Some("dummy")).expect("dummy pin");
        assert_eq!(added.interface_name, "wlan1");
        assert_eq!(added.chipset, BENCH_CHIPSET);
        assert!(added.is_wfb_compatible && added.supports_monitor);

        // Another virtual kind, a physical link, or no pin at all: refused.
        assert_eq!(pinned_dummy_adapter(&none, "wlan1", Some("veth")), None);
        assert_eq!(pinned_dummy_adapter(&none, "wlan1", None), None);
        assert_eq!(pinned_dummy_adapter(&none, "", Some("dummy")), None);

        // A name the scan already reported is never duplicated.
        let scanned = vec![added.clone()];
        assert_eq!(pinned_dummy_adapter(&scanned, "wlan1", Some("dummy")), None);
    }
}
