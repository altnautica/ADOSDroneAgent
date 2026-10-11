use super::*;

fn ap_is_hotspot(name: &str) -> bool {
    name == "hotspot"
}

// ----- candidate classification (connection level) -----

#[test]
fn keeps_only_active_infrastructure_wifi() {
    let terse = "\
Wired connection 1:802-3-ethernet:eth0:activated
home:802-11-wireless:wlan0:activated
lo:loopback:lo:activated
";
    let got = parse_active_wifi_connections(terse, ap_is_hotspot);
    assert_eq!(
        got,
        vec![WifiConnection {
            name: "home".to_string(),
            iface: "wlan0".to_string(),
        }]
    );
}

#[test]
fn drops_non_activated_wifi() {
    // A defined-but-down profile is NetworkManager's job, not ours.
    let terse = "\
home:802-11-wireless:wlan0:activated
backup:802-11-wireless::
office:802-11-wireless:wlan0:deactivated
";
    let got = parse_active_wifi_connections(terse, ap_is_hotspot);
    assert_eq!(
        got.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        vec!["home"]
    );
}

#[test]
fn excludes_access_point_connections() {
    let terse = "\
home:802-11-wireless:wlan0:activated
hotspot:802-11-wireless:wlan0:activated
";
    let got = parse_active_wifi_connections(terse, ap_is_hotspot);
    assert_eq!(
        got.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        vec!["home"]
    );
}

#[test]
fn handles_escaped_colon_in_connection_name() {
    let terse = "my\\:net:802-11-wireless:wlan0:activated\n";
    let got = parse_active_wifi_connections(terse, ap_is_hotspot);
    assert_eq!(got[0].name, "my:net");
    assert_eq!(got[0].iface, "wlan0");
}

#[test]
fn empty_and_short_lines_skipped() {
    let terse = "\n:802-11-wireless:wlan0:activated\nshort\nhome:802-11-wireless:wlan0:activated\n";
    let got = parse_active_wifi_connections(terse, ap_is_hotspot);
    assert_eq!(
        got.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        vec!["home"]
    );
}

#[test]
fn no_wifi_yields_empty() {
    let terse = "Wired connection 1:802-3-ethernet:eth0:activated\nlo:loopback:lo:activated\n";
    assert!(parse_active_wifi_connections(terse, ap_is_hotspot).is_empty());
}

// ----- interface-level exclusion -----

#[test]
fn injection_adapter_is_never_a_candidate() {
    // The WFB injection adapter (RTL family), even reported in managed mode,
    // is never an onboard management link.
    assert!(!iface_is_managed_candidate(
        "rtl88x2eu",
        None,
        Some("managed")
    ));
    assert!(!iface_is_managed_candidate(
        "rtl8812au",
        None,
        Some("monitor")
    ));
    // A flight radio bound to a driver outside the driver table is still
    // recognised by its USB ids.
    let (vid, pid, _) = ados_protocol::wfb_tables::WFB_COMPATIBLE[0];
    assert!(!iface_is_managed_candidate(
        "rtw88_8822bu",
        Some((vid, pid)),
        Some("managed")
    ));
}

#[test]
fn monitor_mode_iface_is_never_a_candidate() {
    // A monitor-mode interface is the radio adapter; never touch it.
    assert!(!iface_is_managed_candidate(
        "brcmfmac",
        None,
        Some("monitor")
    ));
}

#[test]
fn unreadable_mode_is_not_a_candidate() {
    // Fail safe: never act on an interface whose mode we cannot confirm.
    assert!(!iface_is_managed_candidate("brcmfmac", None, None));
}

#[test]
fn onboard_managed_wifi_is_a_candidate() {
    assert!(iface_is_managed_candidate(
        "brcmfmac",
        None,
        Some("managed")
    ));
    assert!(iface_is_managed_candidate(
        "aic8800_fdrv",
        None,
        Some("managed")
    ));
    assert!(iface_is_managed_candidate(
        "brcmfmac",
        None,
        Some("station")
    ));
}

// ----- gateway + neighbor parsing -----

#[test]
fn parses_gateway_from_default_route() {
    let text = "default via 192.168.1.1 proto dhcp src 192.168.1.50 metric 600\n";
    assert_eq!(parse_gateway(text).as_deref(), Some("192.168.1.1"));
}

#[test]
fn no_gateway_when_no_default_route() {
    let text = "192.168.1.0/24 proto kernel scope link src 192.168.1.50\n";
    assert_eq!(parse_gateway(text), None);
}

#[test]
fn neighbor_reachable_states() {
    assert!(parse_neighbor_reachable(
        "192.168.1.1 dev wlan0 lladdr aa:bb:cc:dd:ee:ff REACHABLE\n"
    ));
    assert!(parse_neighbor_reachable(
        "192.168.1.1 dev wlan0 lladdr aa:bb:cc:dd:ee:ff PERMANENT\n"
    ));
}

#[test]
fn an_unconfirmed_neighbor_entry_is_not_a_reachable_gateway() {
    // STALE lingers on an idle link whose gateway stopped answering, and
    // DELAY / PROBE mean the kernel is already doubting the entry.
    for state in ["STALE", "DELAY", "PROBE", "FAILED"] {
        let line = format!("192.168.1.1 dev wlan0 lladdr aa:bb:cc:dd:ee:ff {state}\n");
        assert!(!parse_neighbor_reachable(&line), "{state}");
    }
    assert!(!parse_neighbor_reachable(
        "192.168.1.1 dev wlan0  INCOMPLETE\n"
    ));
    assert!(!parse_neighbor_reachable(""));
}

#[test]
fn access_point_predicate_matches_known_shapes() {
    assert!(looks_like_access_point("ados-hotspot"));
    assert!(looks_like_access_point("ADOS-Hotspot"));
    assert!(looks_like_access_point("my-hotspot"));
    assert!(looks_like_access_point("field-ap"));
    assert!(looks_like_access_point("ap"));
    assert!(!looks_like_access_point("home"));
    // A multi-word infrastructure SSID containing an ampersand must not be
    // mistaken for a hosted access point.
    assert!(!looks_like_access_point("Home & Office"));
}
