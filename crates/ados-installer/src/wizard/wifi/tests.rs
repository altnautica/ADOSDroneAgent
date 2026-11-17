use super::*;

#[test]
fn wifi_iface_names_are_recognized() {
    assert!(is_wifi_iface("wlan0"));
    assert!(is_wifi_iface("wlp3s0"));
    assert!(is_wifi_iface("wlx00c0ca"));
    assert!(!is_wifi_iface("eth0"));
    assert!(!is_wifi_iface("end0"));
    assert!(!is_wifi_iface("lo"));
}

#[test]
fn radio_drivers_are_flagged_case_insensitively() {
    assert!(is_radio_driver("8812eu"));
    assert!(is_radio_driver("RTL8812AU"));
    assert!(is_radio_driver("rtl88x2eu"));
    // The onboard management-WiFi drivers are not long-range radios.
    assert!(!is_radio_driver("aic8800_fdrv"));
    assert!(!is_radio_driver("brcmfmac"));
    assert!(!is_radio_driver("iwlwifi"));
}

#[test]
fn terse_split_honors_backslash_escape() {
    // A colon inside an SSID is backslash-escaped in nmcli terse output.
    let cols = split_terse(r"home\:5G:82:WPA2:*");
    assert_eq!(cols, vec!["home:5G", "82", "WPA2", "*"]);
    // Trailing empty field is preserved.
    assert_eq!(split_terse("open:70::"), vec!["open", "70", "", ""]);
}

#[test]
fn wifi_list_parses_dedups_and_sorts_by_signal() {
    let out = "\
home-5G:82:WPA2:*
home-2G:60:WPA2:
neighbor:45::
home-5G:70:WPA2:
:33:WPA2:
";
    let nets = parse_wifi_list(out);
    // Strongest first; the blank SSID row is dropped; home-5G de-duped to 82.
    assert_eq!(nets.len(), 3);
    assert_eq!(nets[0].ssid, "home-5G");
    assert_eq!(nets[0].signal, 82);
    assert!(nets[0].secured);
    assert!(nets[0].in_use);
    assert_eq!(nets[1].ssid, "home-2G");
    assert_eq!(nets[2].ssid, "neighbor");
    // "neighbor" has empty SECURITY → open.
    assert!(!nets[2].secured);
}

#[test]
fn egress_iface_parsed_from_both_route_forms() {
    // Direct (on-link) form.
    assert_eq!(
        parse_egress_iface("1.1.1.1 dev eth0 src 192.168.1.42 uid 0"),
        Some("eth0".to_string())
    );
    // Via-gateway form.
    assert_eq!(
        parse_egress_iface("1.1.1.1 via 192.168.1.1 dev wlan0 src 192.168.1.50"),
        Some("wlan0".to_string())
    );
    // No `dev` token → no interface.
    assert_eq!(parse_egress_iface("1.1.1.1 unreachable"), None);
}

#[test]
fn default_gateway_parsed() {
    assert_eq!(
        parse_default_gw("default via 192.168.1.1 proto dhcp metric 600"),
        Some("192.168.1.1".to_string())
    );
    // An on-link default route with no gateway → none.
    assert_eq!(parse_default_gw("default dev eth0 proto dhcp"), None);
}

#[test]
fn ipv4_address_parsed_without_its_prefix() {
    let out = "3: wlan0    inet 192.168.1.50/24 brd 192.168.1.255 scope global dynamic noprefixroute wlan0\\       valid_lft 86321sec preferred_lft 86321sec\n";
    assert_eq!(parse_ipv4_addr(out), Some("192.168.1.50".to_string()));
    // No address on the interface: empty output.
    assert_eq!(parse_ipv4_addr(""), None);
}

#[test]
fn wifi_devices_filtered_from_device_status() {
    let out = "\
eth0:ethernet:connected:Wired
wlan0:wifi:disconnected:
wlan1:wifi:connected:home-5G
lo:loopback:unmanaged:
";
    let devs = parse_wifi_devices(out);
    assert_eq!(
        devs,
        vec![
            ("wlan0".to_string(), "disconnected".to_string()),
            ("wlan1".to_string(), "connected".to_string()),
        ]
    );
}
