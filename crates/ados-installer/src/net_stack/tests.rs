use super::adopt::{comment_out_iface, device_state, dhcpcd_deny};
use super::*;

fn links(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(n, s)| (n.to_string(), s.to_string()))
        .collect()
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// An Armbian minimal image: netplan renders every Ethernet to networkd,
/// the onboard Wi-Fi is left unmanaged, NetworkManager is absent.
fn armbian() -> NetStack {
    NetStack {
        netplan_renderer: Some("networkd".into()),
        networkd_links: links(&[
            ("lo", "unmanaged"),
            ("enp1s0", "configured"),
            ("wlan0", "unmanaged"),
            ("tailscale0", "unmanaged"),
        ]),
        wired_ifaces: strs(&["enp1s0"]),
        wireless_ifaces: strs(&["wlan0"]),
        wifi_iface: Some("wlan0".into()),
        ..NetStack::default()
    }
}

#[test]
fn armbian_switches_netplan_and_hands_wired_off_until_the_reboot() {
    let s = armbian();
    assert_eq!(s.label(), "netplan + systemd-networkd");
    let p = plan(&s);
    assert!(p.install_nm && p.start_nm && p.netplan_switch);
    assert_eq!(p.handoff_ifaces, strs(&["enp1s0"]));
    assert!(p.handoff_ephemeral);
    assert!(p.disable_networkd_wait_online);
    assert_eq!(p.networkd_release_wifi, None);
    assert_eq!(p.ifupdown_release_wifi, None);
    assert_eq!(p.dhcpcd_deny_wifi, None);
    assert!(!p.noop());
}

#[test]
fn a_host_already_on_networkmanager_is_a_noop() {
    let s = NetStack {
        nm_installed: true,
        nm_active: true,
        wired_ifaces: strs(&["eth0"]),
        wireless_ifaces: strs(&["wlan0"]),
        wifi_iface: Some("wlan0".into()),
        ..NetStack::default()
    };
    assert_eq!(s.label(), "NetworkManager");
    let p = plan(&s);
    assert!(p.noop(), "{p:?}");
    assert!(!p.netplan_reboot_pending);
}

#[test]
fn dhcpcd_denies_the_wifi_and_keeps_wired_permanently() {
    let s = NetStack {
        dhcpcd_active: true,
        wired_ifaces: strs(&["eth0"]),
        wireless_ifaces: strs(&["wlan0"]),
        wifi_iface: Some("wlan0".into()),
        ..NetStack::default()
    };
    assert_eq!(s.label(), "dhcpcd");
    let p = plan(&s);
    assert_eq!(p.dhcpcd_deny_wifi.as_deref(), Some("wlan0"));
    assert_eq!(p.handoff_ifaces, strs(&["eth0"]));
    assert!(!p.handoff_ephemeral);
    assert!(!p.netplan_switch);
}

#[test]
fn ifupdown_releases_a_declared_wifi() {
    let s = NetStack {
        ifupdown_ifaces: strs(&["eth0", "wlan0"]),
        wired_ifaces: strs(&["eth0"]),
        wireless_ifaces: strs(&["wlan0"]),
        wifi_iface: Some("wlan0".into()),
        ..NetStack::default()
    };
    assert_eq!(s.label(), "ifupdown");
    let p = plan(&s);
    assert_eq!(p.ifupdown_release_wifi.as_deref(), Some("wlan0"));
    assert_eq!(p.handoff_ifaces, strs(&["eth0"]));
}

#[test]
fn networkd_claiming_the_wifi_releases_it() {
    let s = NetStack {
        networkd_links: links(&[("eth0", "configured"), ("wlan0", "configuring")]),
        wired_ifaces: strs(&["eth0"]),
        wireless_ifaces: strs(&["wlan0"]),
        wifi_iface: Some("wlan0".into()),
        wpa_iface_unit_enabled: true,
        ..NetStack::default()
    };
    assert_eq!(s.label(), "systemd-networkd");
    let p = plan(&s);
    assert_eq!(p.networkd_release_wifi.as_deref(), Some("wlan0"));
    assert_eq!(p.disable_wpa_unit.as_deref(), Some("wlan0"));
    assert_eq!(
        p.handoff_ifaces,
        strs(&["eth0"]),
        "the wifi is never handed off"
    );
    assert!(!p.netplan_switch);
}

#[test]
fn a_host_with_only_a_long_range_radio_has_no_wifi_to_release() {
    // The probe never picks a long-range radio as the management Wi-Fi.
    let s = NetStack {
        networkd_links: links(&[("eth0", "configured"), ("wlan1", "unmanaged")]),
        wired_ifaces: strs(&["eth0"]),
        wireless_ifaces: strs(&["wlan1"]),
        wifi_iface: None,
        ..NetStack::default()
    };
    let p = plan(&s);
    assert_eq!(p.wifi_iface, None);
    assert_eq!(p.networkd_release_wifi, None);
    assert_eq!(p.dhcpcd_deny_wifi, None);
    assert_eq!(p.disable_wpa_unit, None);
}

#[test]
fn a_rerun_before_the_reboot_still_asks_for_it() {
    // The switch is on disk and NetworkManager runs the Wi-Fi, but networkd
    // still runs the wired link: the boot that completes it is still owed.
    let mut s = armbian();
    s.nm_installed = true;
    s.nm_active = true;
    s.netplan_renderer = Some("NetworkManager".into());
    s.ados_netplan_written = true;
    let p = plan(&s);
    assert!(p.noop(), "{p:?}");
    assert!(p.netplan_reboot_pending);
    // After the reboot networkd runs nothing, so nothing is owed.
    s.networkd_links = links(&[("enp1s0", "unmanaged")]);
    assert!(!plan(&s).netplan_reboot_pending);
}

#[test]
fn networkctl_list_parses_link_and_setup() {
    let out = "  1 lo         loopback carrier  unmanaged\n  2 enp1s0     ether    routable configured\n  3 wlan0      wlan     off      unmanaged\n";
    assert_eq!(
        parse_networkctl(out),
        links(&[
            ("lo", "unmanaged"),
            ("enp1s0", "configured"),
            ("wlan0", "unmanaged")
        ])
    );
    assert!(parse_networkctl("garbage line\n").is_empty());
}

#[test]
fn ifupdown_parse_skips_loopback_and_repeats() {
    let body = "auto lo\niface lo inet loopback\n\nauto eth0\niface eth0 inet dhcp\niface eth0 inet6 auto\nallow-hotplug wlan0\niface wlan0 inet dhcp\n    wpa-conf /etc/wpa_supplicant/wpa_supplicant.conf\n";
    assert_eq!(parse_ifupdown(body), strs(&["eth0", "wlan0"]));
}

#[test]
fn ifupdown_wifi_stanza_is_commented_out_and_the_rest_kept() {
    let body = "auto lo eth0 wlan0\niface lo inet loopback\nallow-hotplug wlan0\niface wlan0 inet dhcp\n    wpa-conf /etc/wpa.conf\n\niface eth0 inet dhcp\n";
    let out = comment_out_iface(body, "wlan0").unwrap();
    assert_eq!(
        out,
        "#ados-nm# auto lo eth0 wlan0\nauto lo eth0\niface lo inet loopback\n#ados-nm# allow-hotplug wlan0\n#ados-nm# iface wlan0 inet dhcp\n#ados-nm#     wpa-conf /etc/wpa.conf\n#ados-nm# \niface eth0 inet dhcp\n"
    );
    assert_eq!(comment_out_iface("iface eth0 inet dhcp\n", "wlan0"), None);
}

#[test]
fn dhcpcd_deny_is_appended_once() {
    let out = dhcpcd_deny("hostname\n", "wlan0").unwrap();
    assert!(out.ends_with("denyinterfaces wlan0\n"));
    assert_eq!(dhcpcd_deny(&out, "wlan0"), None);
}

#[test]
fn handoff_conf_is_its_own_device_section() {
    let body = render_handoff_conf(&strs(&["enp1s0", "eth1"]));
    assert!(body.starts_with('#'));
    assert!(body.contains(
        "[device-ados-wired-handoff]\nmatch-device=interface-name:enp1s0;interface-name:eth1\nmanaged=0\n"
    ));
    // An `unmanaged-devices` list would be replaced by the WFB rule's list.
    assert!(!body.contains("unmanaged-devices"));
}

#[test]
fn nmcli_device_state_is_read_per_device() {
    let out = "enp1s0:ethernet:unmanaged\nwlan0:wifi:disconnected\n";
    assert_eq!(device_state(out, "wlan0").as_deref(), Some("disconnected"));
    assert_eq!(device_state(out, "wlan1"), None);
}
