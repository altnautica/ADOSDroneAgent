//! Tests for [`super`] (the interface classifier).

use super::*;

fn ifaces(pairs: &[(&str, &str)]) -> Vec<WirelessIface> {
    pairs
        .iter()
        .map(|(n, d)| WirelessIface {
            name: (*n).to_string(),
            driver: (*d).to_string(),
        })
        .collect()
}

#[test]
fn drivers_classify_from_the_generated_tables() {
    assert!(is_injection_driver("rtl88x2eu"));
    assert!(
        is_injection_driver("RTL8812EU"),
        "compare is case-insensitive"
    );
    assert!(!is_injection_driver("brcmfmac"));

    assert!(is_onboard_wifi_driver("brcmfmac"));
    assert!(
        is_onboard_wifi_driver("brcmfmac_sdio"),
        "the deny-set is prefix-matched"
    );
    assert!(is_onboard_wifi_driver("aic8800"));
    assert!(!is_onboard_wifi_driver("rtl88x2eu"));
}

#[test]
fn a_known_radio_id_is_a_wfb_adapter_whatever_driver_it_bound() {
    let (vid, pid, _) = WFB_COMPATIBLE[0];
    // A mainline driver outside the driver table, on a flight-radio id.
    assert!(is_wfb_adapter("rtw88_8822bu", Some((vid, pid))));
    // The driver table still answers when the ids could not be read.
    assert!(is_wfb_adapter("rtl88x2eu", None));
    // Management WiFi is denied before either table is consulted.
    assert!(!is_wfb_adapter("brcmfmac", Some((vid, pid))));
    assert!(!is_wfb_adapter("rtl88x2eu", Some((DENY_VID[0], 0x0001))));
    assert!(!is_wfb_adapter("rtw88_8822bu", Some((0x1234, 0x5678))));
}

#[test]
fn usb_ids_are_read_from_the_parent_device_node() {
    let dir = tempfile::tempdir().unwrap();
    let device = dir.path().join("usb1").join("1-1");
    let interface = device.join("1-1:1.0");
    std::fs::create_dir_all(&interface).unwrap();
    std::fs::write(device.join("idVendor"), "0bda\n").unwrap();
    std::fs::write(device.join("idProduct"), "a81a\n").unwrap();
    let net = dir.path().join("net");
    std::fs::create_dir_all(net.join("wlan1")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&interface, net.join("wlan1").join("device")).unwrap();
    assert_eq!(usb_vid_pid_in(&net, "wlan1"), Some((0x0BDA, 0xA81A)));
    assert_eq!(usb_vid_pid_in(&net, "eth0"), None);
}

/// The bug itself: the same two devices under both name orderings must
/// yield the same answer. Measured on the bench across three reboots.
#[test]
fn the_choice_survives_the_names_racing() {
    let onboard_first = ifaces(&[("wlan0", "brcmfmac"), ("wlan1", "rtl88x2eu")]);
    let radio_first = ifaces(&[("wlan0", "rtl88x2eu"), ("wlan1", "brcmfmac")]);

    assert_eq!(
        choose_ap_interface(&onboard_first, "", None).unwrap(),
        "wlan0"
    );
    assert_eq!(
        choose_ap_interface(&radio_first, "", None).unwrap(),
        "wlan1",
        "when the radio takes the wlan0 name the AP must follow the onboard chip"
    );
}

#[test]
fn the_radios_own_verdict_overrides_classification() {
    // Even if the driver were misclassified, the interface the radio says
    // it took is refused. Belt and braces, because the cost of being wrong
    // is taking down the aircraft's link.
    let list = ifaces(&[("wlan0", "brcmfmac"), ("wlan1", "brcmfmac")]);
    assert_eq!(
        choose_ap_interface(&list, "", Some("wlan0")).unwrap(),
        "wlan1"
    );
}

#[test]
fn a_box_with_only_the_radio_gets_no_access_point() {
    let list = ifaces(&[("wlan0", "rtl88x2eu")]);
    assert_eq!(
        choose_ap_interface(&list, "", None),
        Err(ApIfaceError::NoOnboardWifi)
    );
}

#[test]
fn an_operator_pin_is_honoured_but_never_onto_the_radio() {
    let list = ifaces(&[("wlan0", "brcmfmac"), ("wlan1", "rtl88x2eu")]);
    assert_eq!(choose_ap_interface(&list, "wlan0", None).unwrap(), "wlan0");
    assert!(matches!(
        choose_ap_interface(&list, "wlan1", None),
        Err(ApIfaceError::ConfiguredIsRadio { .. })
    ));
    assert!(matches!(
        choose_ap_interface(&list, "wlan9", None),
        Err(ApIfaceError::ConfiguredMissing { .. })
    ));
}

#[test]
fn the_radio_sidecar_is_read_and_blanks_are_not_a_verdict() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("wfb-stats.json");

    std::fs::write(&p, r#"{"interface":"wlan1","rssi":-36}"#).unwrap();
    assert_eq!(radio_interface_from(&p).as_deref(), Some("wlan1"));

    // A blank or absent field is "the radio has not said", NOT "no radio" —
    // it must not read as a verdict that some interface is free.
    std::fs::write(&p, r#"{"interface":"  "}"#).unwrap();
    assert_eq!(radio_interface_from(&p), None);
    std::fs::write(&p, r#"{"rssi":-36}"#).unwrap();
    assert_eq!(radio_interface_from(&p), None);
    assert_eq!(radio_interface_from(Path::new("/nope/absent.json")), None);
}

#[cfg(unix)]
#[test]
fn enumeration_is_sorted_and_skips_what_it_cannot_identify() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // Two wireless ifaces created out of alphabetical order, plus one with
    // no driver link at all, plus a non-wireless one.
    for (name, driver) in [("wlan1", Some("rtl88x2eu")), ("wlan0", Some("brcmfmac"))] {
        let d = root.join(name);
        std::fs::create_dir_all(d.join("phy80211")).unwrap();
        if let Some(drv) = driver {
            let target = root.join("drivers").join(drv);
            std::fs::create_dir_all(&target).unwrap();
            std::fs::create_dir_all(d.join("device")).unwrap();
            std::os::unix::fs::symlink(&target, d.join("device").join("driver")).unwrap();
        }
    }
    std::fs::create_dir_all(root.join("wlan7").join("phy80211")).unwrap(); // no driver
    std::fs::create_dir_all(root.join("eth0")).unwrap(); // not wireless

    let found = list_wireless_in(root);
    assert_eq!(
        found,
        ifaces(&[("wlan0", "brcmfmac"), ("wlan1", "rtl88x2eu")]),
        "sorted, wireless only, and an unidentifiable iface is dropped"
    );
}

#[cfg(unix)]
#[test]
fn wired_ports_are_classified_by_device_not_name() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // (iface, device path fragment, driver, link type)
    let rigs = [
        ("end1", "platform/fe300000.ethernet", "rk_gmac-dwmac", "1"),
        ("enx001122334455", "usb1/1-1/1-1:1.0", "r8152", "1"),
        ("usb0", "platform/fe980000.usb/gadget", "g_ether", "1"),
        ("eth1", "usb1/1-2/1-2:1.0", "rndis_host", "1"),
        ("wwan0", "usb1/1-3/1-3:1.4", "qmi_wwan", "65534"),
    ];
    for (name, device, driver, link_type) in rigs {
        let dev = root.join("devices").join(device);
        let drv = root.join("drivers").join(driver);
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::create_dir_all(&drv).unwrap();
        std::os::unix::fs::symlink(&drv, dev.join("driver")).unwrap();
        let iface = root.join(name);
        std::fs::create_dir_all(&iface).unwrap();
        std::fs::write(iface.join("type"), format!("{link_type}\n")).unwrap();
        std::os::unix::fs::symlink(&dev, iface.join("device")).unwrap();
    }
    // A virtual bridge: Ethernet type, no backing device.
    std::fs::create_dir_all(root.join("br0")).unwrap();
    std::fs::write(root.join("br0").join("type"), "1\n").unwrap();

    assert_eq!(
        list_wired_in(root),
        vec!["end1".to_string(), "enx001122334455".to_string()],
        "gadget, tether, modem and virtual interfaces are not wired ports"
    );
}

/// One netdev under `root`: link type, a backing device at `device` (relative
/// to `root/devices`) bound to `driver`, optional USB ids on the device node's
/// parent, and an 802.11 marker when `wireless`.
#[cfg(unix)]
fn rig(
    root: &Path,
    name: &str,
    device: &str,
    driver: &str,
    ids: Option<(&str, &str)>,
    wireless: bool,
) {
    let dev = root.join("devices").join(device);
    let drv = root.join("drivers").join(driver);
    std::fs::create_dir_all(&dev).unwrap();
    std::fs::create_dir_all(&drv).unwrap();
    std::os::unix::fs::symlink(&drv, dev.join("driver")).unwrap();
    if let Some((vid, pid)) = ids {
        let parent = dev.parent().unwrap();
        std::fs::write(parent.join("idVendor"), format!("{vid}\n")).unwrap();
        std::fs::write(parent.join("idProduct"), format!("{pid}\n")).unwrap();
    }
    let iface = root.join("net").join(name);
    std::fs::create_dir_all(&iface).unwrap();
    std::fs::write(iface.join("type"), "1\n").unwrap();
    if wireless {
        std::fs::create_dir_all(iface.join("phy80211")).unwrap();
    }
    std::os::unix::fs::symlink(&dev, iface.join("device")).unwrap();
}

#[cfg(unix)]
#[test]
fn mesh_capable_two_rtl_usb_true() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    rig(
        root,
        "wlan1",
        "usb1/1-1/1-1:1.0",
        "rtl88x2eu",
        Some(("0bda", "a81a")),
        true,
    );
    rig(
        root,
        "wlan2",
        "usb1/1-2/1-2:1.0",
        "rtl88x2eu",
        Some(("0bda", "a81a")),
        true,
    );
    assert!(mesh_capable_in(&root.join("net")));
}

#[cfg(unix)]
#[test]
fn mesh_capable_rtl_plus_brcmfmac_false() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    rig(
        root,
        "wlan0",
        "platform/mmc1/mmc1:0001:1",
        "brcmfmac",
        None,
        true,
    );
    rig(
        root,
        "wlan1",
        "usb1/1-1/1-1:1.0",
        "rtl88x2eu",
        Some(("0bda", "a81a")),
        true,
    );
    assert!(!mesh_capable_in(&root.join("net")));
}

#[cfg(unix)]
#[test]
fn cellular_qmi_named_wwp_found() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    rig(
        root,
        "wwp0s20u4i4",
        "usb1/1-4/1-4:1.4",
        "qmi_wwan",
        Some(("2c7c", "0125")),
        false,
    );
    rig(
        root,
        "eth0",
        "platform/fe300000.ethernet",
        "stmmaceth",
        None,
        false,
    );
    // `option` on a non-modem vendor is a generic serial device, not an uplink.
    rig(
        root,
        "ttyx0",
        "usb1/1-5/1-5:1.0",
        "option",
        Some(("1234", "5678")),
        false,
    );
    rig(
        root,
        "wwan1",
        "usb1/1-6/1-6:1.0",
        "option",
        Some(("1199", "9071")),
        false,
    );
    assert_eq!(
        list_cellular_in(&root.join("net")),
        vec!["wwan1".to_string(), "wwp0s20u4i4".to_string()]
    );
}

#[cfg(unix)]
#[test]
fn usb0_gadget_excluded() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    rig(
        root,
        "usb0",
        "platform/fe980000.usb/gadget",
        "cdc_ncm",
        None,
        false,
    );
    rig(
        root,
        "usb1",
        "platform/fe980000.udc/net",
        "rndis_host",
        None,
        false,
    );
    assert!(list_cellular_in(&root.join("net")).is_empty());
}
