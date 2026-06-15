//! Link-health detection + management-interface resolution for the guardian.
//!
//! The three-signal health check (carrier, routable lease, gateway reachability)
//! and the interface picker are pure and unit-tested on every host. The Linux
//! signal-collection edges (sysfs carrier read, `ip -4 addr`, `ip -4 route`,
//! `ip neighbor`, the driver / wireless sysfs reads) are `#[cfg(target_os =
//! "linux")]` and absent on a non-Linux dev host.

/// The three-signal verdict on the operator's management link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthVerdict {
    /// Carrier up, a routable lease, and the gateway answers: a working path.
    Healthy,
    /// Carrier up + a lease, but the gateway does not answer ARP — the link is
    /// up yet passes no traffic (the foreign-domain / dead-data-path case).
    Degraded,
    /// No carrier or no routable lease: the link is physically or
    /// configurationally down.
    Down,
}

/// The three independent link signals folded into a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkSignals {
    pub carrier: bool,
    pub has_lease: bool,
    pub gateway_reachable: bool,
}

/// Pure verdict. `Down` dominates: a missing carrier or lease is `Down`
/// regardless of the gateway; carrier + lease with an unreachable gateway is
/// `Degraded` (up-but-no-data-path); all three good is `Healthy`.
pub fn verdict_of(s: LinkSignals) -> HealthVerdict {
    if !s.carrier || !s.has_lease {
        HealthVerdict::Down
    } else if !s.gateway_reachable {
        HealthVerdict::Degraded
    } else {
        HealthVerdict::Healthy
    }
}

/// The transport class of a management interface, used to drop the Wi-Fi-only
/// rung on a wired link and to label the heartbeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Ethernet,
    Wifi,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Ethernet => "ethernet",
            Transport::Wifi => "wifi",
        }
    }
}

/// A resolved management interface: the interface name + its transport class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedIface {
    pub iface: String,
    pub transport: Transport,
}

/// One physical interface the picker considers as a management-link candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfaceCandidate {
    pub name: String,
    pub transport: Transport,
    /// True when this is the WFB injection adapter (a Realtek monitor-mode
    /// radio) — never a management link, never to be touched by a repair.
    pub is_injection: bool,
    /// True when this is a virtual / loopback / mesh-carrier interface.
    pub is_virtual: bool,
}

// ---------------------------------------------------------------------------
// Pure parsing + classification (unit-tested on every host)
// ---------------------------------------------------------------------------

/// True when an interface's `carrier` sysfs file reads `1` (link up). A `0` or
/// an unreadable file (admin-down interface) is not a carrier. Pure.
pub fn parse_carrier(text: &str) -> bool {
    text.trim() == "1"
}

/// True when `ip -4 addr show dev <if>` output carries a routable IPv4 address:
/// an `inet` line whose address is not loopback (`127.`) and not link-local
/// (`169.254.`). Pure.
pub fn parse_has_routable_ipv4(text: &str) -> bool {
    for line in text.lines() {
        let t = line.trim();
        let Some(rest) = t.strip_prefix("inet ") else {
            continue;
        };
        let addr = rest.split('/').next().unwrap_or("").trim();
        if addr.is_empty() || addr.starts_with("127.") || addr.starts_with("169.254.") {
            continue;
        }
        return true;
    }
    false
}

/// Parse the interface carrying the kernel default route out of `ip route`
/// output: the first `dev <iface>` on a `default ...` line, or `None`. Pure.
pub fn parse_default_route_iface(text: &str) -> Option<String> {
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

/// Parse the gateway out of `ip -4 route show default ...` output: the `via
/// <gw>` token on the `default` line, or `None`. Pure.
pub fn parse_gateway(text: &str) -> Option<String> {
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.first() == Some(&"default") {
            if let Some(idx) = parts.iter().position(|p| *p == "via") {
                if let Some(gw) = parts.get(idx + 1) {
                    return Some((*gw).to_string());
                }
            }
        }
    }
    None
}

/// Parse gateway reachability from `ip neighbor show <gw> dev <if>` output. A
/// usable cached neighbour (REACHABLE / STALE / DELAY / PROBE) means the gateway
/// answers ARP; INCOMPLETE / FAILED / an absent entry means the data path is
/// dead. Pure.
pub fn parse_neighbor_reachable(text: &str) -> bool {
    for line in text.lines() {
        let upper = line.to_ascii_uppercase();
        if upper.contains("REACHABLE")
            || upper.contains("STALE")
            || upper.contains("DELAY")
            || upper.contains("PROBE")
        {
            return true;
        }
    }
    false
}

/// Whether to confirm a passive "gateway unreachable" with an active ping. A
/// passive neighbour-cache read reports a stale or absent entry as unreachable
/// even when the gateway answers; on an otherwise-up link (carrier + lease) that
/// is exactly the false-negative an active probe resolves, so the guardian is
/// not pinned in `Degraded` after its own repair churns the neighbour entry with
/// no traffic to re-resolve it. A down link (no carrier / no lease) is `Down`
/// regardless and needs no probe. Pure.
pub fn should_active_probe_gateway(
    passive_reachable: bool,
    carrier: bool,
    has_lease: bool,
) -> bool {
    !passive_reachable && carrier && has_lease
}

/// True when a kernel driver name denotes the WFB injection adapter (a
/// Realtek monitor-mode radio), which is never a management link. Lower-cased
/// compare; matches the radio adapter selection's compatible-driver set. Pure.
pub fn is_injection_driver(driver: &str) -> bool {
    const INJECTION_DRIVERS: &[&str] = &[
        "8812au",
        "8812eu",
        "rtl8812au",
        "rtl8812eu",
        "rtl88x2eu",
        "rtl88xxau",
    ];
    let d = driver.trim().to_ascii_lowercase();
    INJECTION_DRIVERS.contains(&d.as_str())
}

/// True when an interface NAME denotes a wireless device by the conventional
/// kernel prefixes (`wlan*`, the predictable `wlp*` / `wlx*`, and `wwan*`).
/// Pure. Used as the authoritative first signal for wireless classification so
/// the WFB injection radio is never misclassified as a wired management primary
/// when its `wireless` / `phy80211` sysfs nodes are transiently unreadable
/// during monitor-mode bring-up and regulatory-domain churn.
pub fn is_wireless_name(iface: &str) -> bool {
    iface.starts_with("wlan")
        || iface.starts_with("wlp")
        || iface.starts_with("wlx")
        || iface.starts_with("wwan")
}

/// True when an interface name denotes a virtual / loopback / mesh-carrier
/// device that is never the operator's management link. The USB-tether `usb*`
/// netdev is deliberately NOT excluded (it is a real management path on the
/// ground station). Pure.
pub fn is_virtual_or_loopback(iface: &str) -> bool {
    let n = iface;
    n == "lo"
        || n.starts_with("docker")
        || n.starts_with("veth")
        || n.starts_with("br-")
        || n.starts_with("virbr")
        || n.starts_with("wg")
        || n.starts_with("tun")
        || n.starts_with("tap")
        || n.starts_with("bond")
        || n.starts_with("dummy")
        || n.starts_with("bat")
        || n.starts_with("nm-")
        || n.starts_with("vnet")
}

/// Pick the management interface, given the current default-route interface, the
/// last-known management interface, and the live candidate set. Prefers the
/// current default-route holder; falls back to the last-known one (when the
/// route is momentarily gone but the interface still exists); else a structural
/// pick (wired Ethernet over Wi-Fi). Never returns an injection or virtual
/// interface. Pure.
pub fn pick_managed_iface(
    default_route_iface: Option<&str>,
    last_known: Option<&str>,
    candidates: &[IfaceCandidate],
) -> Option<ManagedIface> {
    let valid = |name: &str| {
        candidates
            .iter()
            .find(|c| c.name == name && !c.is_injection && !c.is_virtual)
    };
    if let Some(d) = default_route_iface {
        if let Some(c) = valid(d) {
            return Some(ManagedIface {
                iface: c.name.clone(),
                transport: c.transport,
            });
        }
    }
    if let Some(l) = last_known {
        if let Some(c) = valid(l) {
            return Some(ManagedIface {
                iface: c.name.clone(),
                transport: c.transport,
            });
        }
    }
    candidates
        .iter()
        .filter(|c| !c.is_injection && !c.is_virtual)
        .min_by_key(|c| match c.transport {
            Transport::Ethernet => 0u8,
            Transport::Wifi => 1u8,
        })
        .map(|c| ManagedIface {
            iface: c.name.clone(),
            transport: c.transport,
        })
}

// ---------------------------------------------------------------------------
// Linux OS edges
// ---------------------------------------------------------------------------

/// Collect the three link signals for an interface. Reads the carrier sysfs
/// file, `ip -4 addr` for a routable lease, and the interface's default-route
/// gateway then its neighbour entry. All reads are read-only.
#[cfg(target_os = "linux")]
pub async fn collect_signals(iface: &str) -> LinkSignals {
    let carrier = tokio::fs::read_to_string(format!("/sys/class/net/{}/carrier", iface))
        .await
        .map(|s| parse_carrier(&s))
        .unwrap_or(false);
    let has_lease = match super::run_output("ip", &["-4", "addr", "show", "dev", iface]).await {
        Some(o) => parse_has_routable_ipv4(&o),
        None => false,
    };
    let gateway =
        match super::run_output("ip", &["-4", "route", "show", "default", "dev", iface]).await {
            Some(o) => parse_gateway(&o),
            None => None,
        };
    // Passive neighbour-cache read first (cheap, generates no traffic).
    let passive_reachable = match &gateway {
        Some(g) => match super::run_output("ip", &["neighbor", "show", g, "dev", iface]).await {
            Some(o) => parse_neighbor_reachable(&o),
            None => false,
        },
        None => false,
    };
    // A passive "unreachable" on an otherwise-up link is the stale/absent-ARP
    // false-negative that can pin the guardian in Degraded after its own repair
    // churned the neighbour entry with no traffic to re-resolve it. Confirm with
    // one active, interface-bound ping (which forces ARP re-resolution) before
    // trusting the passive verdict.
    let gateway_reachable = if should_active_probe_gateway(passive_reachable, carrier, has_lease) {
        match &gateway {
            Some(g) => active_gateway_probe(iface, g).await,
            None => false,
        }
    } else {
        passive_reachable
    };
    LinkSignals {
        carrier,
        has_lease,
        gateway_reachable,
    }
}

/// Actively confirm gateway reachability with a single interface-bound ping.
/// Forces ARP (re-)resolution so a stale or absent neighbour cache cannot report
/// a live gateway as dead — the false-negative that otherwise pins the guardian
/// in `Degraded` after a repair churns the neighbour entry. One ICMP echo with a
/// 1 s timeout; false when ping is unavailable or the gateway stays silent.
/// Read-only with respect to configuration.
#[cfg(target_os = "linux")]
async fn active_gateway_probe(iface: &str, gateway: &str) -> bool {
    super::run_status("ping", &["-c", "1", "-W", "1", "-I", iface, gateway]).await
}

/// The interface carrying the kernel default route, or `None`. Read-only.
#[cfg(target_os = "linux")]
pub async fn default_route_iface() -> Option<String> {
    let out = super::run_output("ip", &["-4", "route", "show", "default"]).await?;
    parse_default_route_iface(&out)
}

/// Enumerate the physical interfaces under `/sys/class/net`, classifying each as
/// injection / virtual and reading its transport. Read-only.
#[cfg(target_os = "linux")]
pub async fn collect_candidates() -> Vec<IfaceCandidate> {
    let mut out = Vec::new();
    let Ok(mut rd) = tokio::fs::read_dir("/sys/class/net").await else {
        return out;
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.is_empty() {
            continue;
        }
        let is_virtual = is_virtual_or_loopback(&name);
        let driver = driver_name(&name).await;
        let is_injection = is_injection_driver(&driver);
        let transport = if is_wireless(&name).await {
            Transport::Wifi
        } else {
            Transport::Ethernet
        };
        out.push(IfaceCandidate {
            name,
            transport,
            is_injection,
            is_virtual,
        });
    }
    out
}

/// Read the kernel driver bound to an interface from
/// `/sys/class/net/<if>/device/driver` (a symlink ending with the driver name).
/// Empty when it cannot be read (a virtual interface has no backing device).
#[cfg(target_os = "linux")]
async fn driver_name(iface: &str) -> String {
    let link = format!("/sys/class/net/{}/device/driver", iface);
    tokio::fs::read_link(&link)
        .await
        .ok()
        .and_then(|p| p.file_name().map(|f| f.to_string_lossy().to_string()))
        .unwrap_or_default()
}

/// True when an interface is wireless. The interface name is the authoritative
/// first signal (so a transiently-unreadable sysfs during monitor-mode churn
/// can never demote the injection radio to "wired"); the `wireless` /
/// `phy80211` sysfs nodes are the fallback for non-conventionally-named NICs.
#[cfg(target_os = "linux")]
async fn is_wireless(iface: &str) -> bool {
    is_wireless_name(iface)
        || tokio::fs::metadata(format!("/sys/class/net/{}/wireless", iface))
            .await
            .is_ok()
        || tokio::fs::metadata(format!("/sys/class/net/{}/phy80211", iface))
            .await
            .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(c: bool, l: bool, g: bool) -> LinkSignals {
        LinkSignals {
            carrier: c,
            has_lease: l,
            gateway_reachable: g,
        }
    }

    #[test]
    fn verdict_down_dominates() {
        // No carrier → Down regardless of the other two.
        assert_eq!(verdict_of(sig(false, true, true)), HealthVerdict::Down);
        assert_eq!(verdict_of(sig(false, false, false)), HealthVerdict::Down);
        // No lease → Down even with carrier.
        assert_eq!(verdict_of(sig(true, false, true)), HealthVerdict::Down);
    }

    #[test]
    fn verdict_degraded_is_up_but_no_data_path() {
        // Carrier + lease but the gateway does not answer → Degraded.
        assert_eq!(verdict_of(sig(true, true, false)), HealthVerdict::Degraded);
    }

    #[test]
    fn verdict_healthy_when_all_good() {
        assert_eq!(verdict_of(sig(true, true, true)), HealthVerdict::Healthy);
    }

    #[test]
    fn carrier_parse() {
        assert!(parse_carrier("1\n"));
        assert!(!parse_carrier("0\n"));
        assert!(!parse_carrier(""));
    }

    #[test]
    fn routable_ipv4_excludes_loopback_and_link_local() {
        assert!(parse_has_routable_ipv4(
            "    inet 192.168.200.50/24 brd 192.168.200.255 scope global dynamic eth0\n"
        ));
        assert!(!parse_has_routable_ipv4(
            "    inet 127.0.0.1/8 scope host lo\n"
        ));
        assert!(!parse_has_routable_ipv4(
            "    inet 169.254.10.2/16 brd 169.254.255.255 scope link\n"
        ));
        assert!(!parse_has_routable_ipv4(""));
    }

    #[test]
    fn default_route_iface_and_gateway() {
        let text = "default via 192.168.200.1 dev eth0 proto dhcp src 192.168.200.50 metric 100\n";
        assert_eq!(parse_default_route_iface(text).as_deref(), Some("eth0"));
        assert_eq!(parse_gateway(text).as_deref(), Some("192.168.200.1"));
        // No default route → both None.
        let none = "192.168.200.0/24 proto kernel scope link src 192.168.200.50\n";
        assert_eq!(parse_default_route_iface(none), None);
        assert_eq!(parse_gateway(none), None);
    }

    #[test]
    fn neighbor_reachable_states() {
        assert!(parse_neighbor_reachable(
            "192.168.200.1 dev eth0 lladdr aa:bb:cc:dd:ee:ff REACHABLE\n"
        ));
        assert!(parse_neighbor_reachable(
            "192.168.200.1 dev eth0 lladdr aa:bb:cc:dd:ee:ff STALE\n"
        ));
        assert!(!parse_neighbor_reachable(
            "192.168.200.1 dev eth0  INCOMPLETE\n"
        ));
        assert!(!parse_neighbor_reachable(
            "192.168.200.1 dev eth0 lladdr aa:bb:cc:dd:ee:ff FAILED\n"
        ));
        assert!(!parse_neighbor_reachable(""));
    }

    #[test]
    fn active_probe_only_on_up_link_with_passive_miss() {
        // Stale/absent-ARP false-negative on an up link → confirm actively.
        assert!(should_active_probe_gateway(false, true, true));
        // Passive already reachable → no active probe needed.
        assert!(!should_active_probe_gateway(true, true, true));
        // A down link (no carrier or no lease) is Down regardless → no probe.
        assert!(!should_active_probe_gateway(false, false, true));
        assert!(!should_active_probe_gateway(false, true, false));
    }

    #[test]
    fn injection_driver_detection() {
        assert!(is_injection_driver("rtl88x2eu"));
        assert!(is_injection_driver("8812eu"));
        assert!(is_injection_driver("RTL8812AU"));
        assert!(!is_injection_driver("brcmfmac"));
        assert!(!is_injection_driver("aic8800_fdrv"));
        assert!(!is_injection_driver(""));
    }

    #[test]
    fn virtual_iface_detection() {
        assert!(is_virtual_or_loopback("lo"));
        assert!(is_virtual_or_loopback("docker0"));
        assert!(is_virtual_or_loopback("veth1234"));
        assert!(is_virtual_or_loopback("br-abc"));
        assert!(is_virtual_or_loopback("wg0"));
        assert!(is_virtual_or_loopback("bat0"));
        // Real management interfaces, including the USB tether, are not virtual.
        assert!(!is_virtual_or_loopback("eth0"));
        assert!(!is_virtual_or_loopback("end1"));
        assert!(!is_virtual_or_loopback("wlan0"));
        assert!(!is_virtual_or_loopback("usb0"));
    }

    #[test]
    fn wireless_name_detection() {
        // The injection radio and onboard WiFi are wireless by name, so a
        // transiently-unreadable sysfs can never classify them as wired.
        assert!(is_wireless_name("wlan0"));
        assert!(is_wireless_name("wlan1"));
        assert!(is_wireless_name("wlp2s0"));
        assert!(is_wireless_name("wlx00c0caaa1111"));
        assert!(is_wireless_name("wwan0"));
        // Wired and other interfaces are not wireless by name.
        assert!(!is_wireless_name("eth0"));
        assert!(!is_wireless_name("end1"));
        assert!(!is_wireless_name("usb0"));
        assert!(!is_wireless_name("lo"));
    }

    fn cand(name: &str, transport: Transport, inj: bool, virt: bool) -> IfaceCandidate {
        IfaceCandidate {
            name: name.to_string(),
            transport,
            is_injection: inj,
            is_virtual: virt,
        }
    }

    #[test]
    fn picker_prefers_default_route_holder() {
        let cands = vec![
            cand("eth0", Transport::Ethernet, false, false),
            cand("wlan0", Transport::Wifi, false, false),
            cand("wlan1", Transport::Wifi, true, false), // injection
            cand("lo", Transport::Ethernet, false, true),
        ];
        let got = pick_managed_iface(Some("wlan0"), None, &cands).unwrap();
        assert_eq!(got.iface, "wlan0");
        assert_eq!(got.transport, Transport::Wifi);
    }

    #[test]
    fn picker_never_returns_injection_or_virtual() {
        let cands = vec![
            cand("wlan1", Transport::Wifi, true, false),
            cand("lo", Transport::Ethernet, false, true),
            cand("eth0", Transport::Ethernet, false, false),
        ];
        // Even if the (impossible) default route names the injection iface, it is
        // never returned; the structural pick lands on eth0.
        let got = pick_managed_iface(Some("wlan1"), None, &cands).unwrap();
        assert_eq!(got.iface, "eth0");
    }

    #[test]
    fn picker_falls_back_to_last_known_then_structural() {
        let cands = vec![
            cand("eth0", Transport::Ethernet, false, false),
            cand("wlan0", Transport::Wifi, false, false),
        ];
        // No current default route, but the last-known iface still exists.
        let got = pick_managed_iface(None, Some("wlan0"), &cands).unwrap();
        assert_eq!(got.iface, "wlan0");
        // Neither default nor last-known → structural pick prefers Ethernet.
        let got = pick_managed_iface(None, None, &cands).unwrap();
        assert_eq!(got.iface, "eth0");
    }

    #[test]
    fn picker_returns_none_with_no_real_iface() {
        let cands = vec![
            cand("lo", Transport::Ethernet, false, true),
            cand("wlan1", Transport::Wifi, true, false),
        ];
        assert_eq!(pick_managed_iface(None, None, &cands), None);
    }
}
