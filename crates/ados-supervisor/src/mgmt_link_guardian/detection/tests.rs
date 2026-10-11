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
        "    inet 192.168.1.50/24 brd 192.168.1.255 scope global dynamic eth0\n"
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
    let text = "default via 192.168.1.1 dev eth0 proto dhcp src 192.168.1.50 metric 100\n";
    assert_eq!(parse_default_route_iface(text).as_deref(), Some("eth0"));
    assert_eq!(parse_gateway(text).as_deref(), Some("192.168.1.1"));
    // No default route → both None.
    let none = "192.168.1.0/24 proto kernel scope link src 192.168.1.50\n";
    assert_eq!(parse_default_route_iface(none), None);
    assert_eq!(parse_gateway(none), None);
}

#[test]
fn neighbor_reachable_states() {
    assert!(parse_neighbor_reachable(
        "192.168.1.1 dev eth0 lladdr aa:bb:cc:dd:ee:ff REACHABLE\n"
    ));
    assert!(parse_neighbor_reachable(
        "192.168.1.1 dev eth0 lladdr aa:bb:cc:dd:ee:ff STALE\n"
    ));
    assert!(!parse_neighbor_reachable(
        "192.168.1.1 dev eth0  INCOMPLETE\n"
    ));
    assert!(!parse_neighbor_reachable(
        "192.168.1.1 dev eth0 lladdr aa:bb:cc:dd:ee:ff FAILED\n"
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
