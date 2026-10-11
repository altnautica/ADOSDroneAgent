use super::*;

#[test]
fn reassert_and_exhausted_have_no_command() {
    for b in [
        Backend::NetworkManager,
        Backend::Networkd,
        Backend::Fallback,
    ] {
        assert!(rung_command(b, RepairRung::ReassertReg, "eth0", Transport::Ethernet).is_empty());
        assert!(rung_command(b, RepairRung::Exhausted, "eth0", Transport::Ethernet).is_empty());
    }
}

#[test]
fn renew_dhcp_matrix() {
    assert_eq!(
        rung_command(
            Backend::NetworkManager,
            RepairRung::RenewDhcp,
            "wlan0",
            Transport::Wifi
        ),
        vec![vec![
            "nmcli".to_string(),
            "device".to_string(),
            "reapply".to_string(),
            "wlan0".to_string()
        ]]
    );
    assert_eq!(
        rung_command(
            Backend::Networkd,
            RepairRung::RenewDhcp,
            "eth0",
            Transport::Ethernet
        ),
        vec![vec![
            "networkctl".to_string(),
            "renew".to_string(),
            "eth0".to_string()
        ]]
    );
    // Fallback issues a release then a fresh request.
    assert_eq!(
        rung_command(
            Backend::Fallback,
            RepairRung::RenewDhcp,
            "eth0",
            Transport::Ethernet
        )
        .len(),
        2
    );
}

#[test]
fn bounce_is_atomic_down_then_up() {
    let nd = rung_command(
        Backend::Networkd,
        RepairRung::BounceIface,
        "eth0",
        Transport::Ethernet,
    );
    assert_eq!(nd.len(), 1);
    assert_eq!(nd[0][0], "sh");
    assert_eq!(nd[0][1], "-c");
    // One atomic command: the down and the up are in the same shell so the
    // local daemon always completes the up.
    assert!(nd[0][2].contains("ip link set eth0 down && ip link set eth0 up"));
    assert!(nd[0][2].contains("networkctl reconfigure eth0"));

    let fb = rung_command(
        Backend::Fallback,
        RepairRung::BounceIface,
        "eth0",
        Transport::Ethernet,
    );
    assert!(fb[0][2].contains("ip link set eth0 down && ip link set eth0 up"));

    let nm = rung_command(
        Backend::NetworkManager,
        RepairRung::BounceIface,
        "wlan0",
        Transport::Wifi,
    );
    assert!(nm[0][2].contains("nmcli device disconnect wlan0"));
    assert!(nm[0][2].contains("nmcli device connect wlan0"));
}

#[test]
fn reconnect_wifi_matrix() {
    assert_eq!(
        rung_command(
            Backend::NetworkManager,
            RepairRung::ReconnectWifi,
            "wlan0",
            Transport::Wifi
        )[0],
        vec![
            "nmcli".to_string(),
            "device".to_string(),
            "reconnect".to_string(),
            "wlan0".to_string()
        ]
    );
    assert_eq!(
        rung_command(
            Backend::Networkd,
            RepairRung::ReconnectWifi,
            "wlan0",
            Transport::Wifi
        )[0],
        vec![
            "wpa_cli".to_string(),
            "-i".to_string(),
            "wlan0".to_string(),
            "reconnect".to_string()
        ]
    );
}

#[test]
fn restart_backend_matrix_adds_wpa_on_wifi_networkd() {
    assert_eq!(
        rung_command(
            Backend::NetworkManager,
            RepairRung::RestartBackend,
            "wlan0",
            Transport::Wifi
        ),
        vec![vec![
            "systemctl".to_string(),
            "restart".to_string(),
            "NetworkManager".to_string()
        ]]
    );
    // networkd + Wi-Fi also restarts the per-iface supplicant.
    let nd_wifi = rung_command(
        Backend::Networkd,
        RepairRung::RestartBackend,
        "wlan0",
        Transport::Wifi,
    );
    assert_eq!(nd_wifi.len(), 2);
    assert_eq!(nd_wifi[1][2], "wpa_supplicant@wlan0");
    // networkd + Ethernet does not.
    let nd_eth = rung_command(
        Backend::Networkd,
        RepairRung::RestartBackend,
        "eth0",
        Transport::Ethernet,
    );
    assert_eq!(nd_eth.len(), 1);
}

#[test]
fn backend_strings_are_bland() {
    assert_eq!(Backend::NetworkManager.as_str(), "networkmanager");
    assert_eq!(Backend::Networkd.as_str(), "networkd");
    assert_eq!(Backend::Fallback.as_str(), "fallback");
}
