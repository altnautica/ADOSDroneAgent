//! Adapter scan + classification tests.

use super::*;

#[test]
fn deny_aic8800_by_driver() {
    assert!(is_denied("aic8800fwh", None));
    assert!(is_denied("brcmfmac", None));
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
fn vid_pid_parent_walk_finds_ids_on_parent_device() {
    // Model the sysfs shape: the resolved `device` node is the USB
    // *interface* (`…:1.0`), which has bInterfaceClass but no id files; the
    // ids live on its parent USB *device* node. The walk must climb to it.
    let dir = tempfile::tempdir().unwrap();
    let device = dir.path().join("1-1");
    let interface = device.join("1-1:1.0");
    std::fs::create_dir_all(&interface).unwrap();
    // Interface node: a class file but NO ids (the trap that returned None).
    std::fs::write(interface.join("bInterfaceClass"), "ff\n").unwrap();
    // Parent device node: the real ids.
    std::fs::write(device.join("idVendor"), "0bda\n").unwrap();
    std::fs::write(device.join("idProduct"), "a81a\n").unwrap();

    assert_eq!(vid_pid_from_device_dir(&interface), Some((0x0BDA, 0xA81A)));
}

#[test]
fn usb_path_walk_names_the_usb_device_node() {
    // The interface node (`1-1.2:1.0`) has no ids; its parent device node
    // (`1-1.2`) does, and its directory name is the stable port path.
    let dir = tempfile::tempdir().unwrap();
    let device = dir.path().join("1-1.2");
    let interface = device.join("1-1.2:1.0");
    std::fs::create_dir_all(&interface).unwrap();
    std::fs::write(device.join("idVendor"), "0bda\n").unwrap();
    assert_eq!(
        usb_path_from_device_dir(&interface).as_deref(),
        Some("1-1.2")
    );
    let bare = dir.path().join("x").join("y");
    std::fs::create_dir_all(&bare).unwrap();
    assert_eq!(usb_path_from_device_dir(&bare), None);
}

#[test]
fn vid_pid_walk_returns_none_when_no_ancestor_has_ids() {
    // A tree with no id files anywhere within the bound yields None rather
    // than climbing out to the filesystem root.
    let dir = tempfile::tempdir().unwrap();
    let leaf = dir.path().join("a").join("b").join("c");
    std::fs::create_dir_all(&leaf).unwrap();
    assert_eq!(vid_pid_from_device_dir(&leaf), None);
}

#[test]
fn speed_parent_walk_reads_speed_off_the_device_node() {
    // `speed` lives on the USB device node (same as the ids), one level up
    // from the interface node the `device` symlink resolves to.
    let dir = tempfile::tempdir().unwrap();
    let device = dir.path().join("1-1");
    let interface = device.join("1-1:1.0");
    std::fs::create_dir_all(&interface).unwrap();
    std::fs::write(device.join("speed"), "480\n").unwrap();
    assert_eq!(speed_from_device_dir(&interface), Some(480));

    // A full-speed enumeration (the degraded RTL path) reads back as 12.
    let dir2 = tempfile::tempdir().unwrap();
    let dev2 = dir2.path().join("8-1");
    std::fs::create_dir_all(&dev2).unwrap();
    std::fs::write(dev2.join("speed"), "12\n").unwrap();
    assert_eq!(speed_from_device_dir(&dev2), Some(12));
}

#[test]
fn usb_speed_degraded_flags_full_speed_only() {
    assert!(usb_speed_degraded(Some(12))); // full-speed RTL = degraded
    assert!(!usb_speed_degraded(Some(480))); // high-speed = fine
    assert!(!usb_speed_degraded(Some(5000))); // SuperSpeed = fine
    assert!(!usb_speed_degraded(None)); // non-USB / unknown is not flagged
}

#[test]
fn parse_supported_modes_extracts_bulleted_modes() {
    let info = "\
Wiphy phy0
	Supported interface modes:
		 * IBSS
		 * managed
		 * monitor
	Band 1:
		Frequencies:
";
    let modes = parse_supported_modes(info);
    assert_eq!(modes, vec!["IBSS", "managed", "monitor"]);
}

#[test]
fn parse_supported_modes_absent_is_empty() {
    assert!(parse_supported_modes("Wiphy phy0\n\tBand 1:\n").is_empty());
    assert!(parse_supported_modes("").is_empty());
}

// ── VID:PID classification table ─────────────────────────────────────────

#[test]
fn vid_pid_table_matches_python_compatible_set() {
    // Every (VID, PID) the Python WFB_COMPATIBLE table carries must classify
    // as a known WFB adapter with the exact same label.
    let cases: &[(u16, u16, &str)] = &[
        (0x0BDA, 0x8812, "RTL8812AU"),
        (0x0BDA, 0x881A, "RTL8812AU (alt)"),
        (0x0BDA, 0x881B, "RTL8812AU (alt)"),
        (0x0BDA, 0x881C, "RTL8812AU (alt)"),
        (0x0BDA, 0xA81A, "RTL8812AU (a81a)"),
        (0x0BDA, 0xB812, "RTL8812EU"),
        (0x2357, 0x0120, "RTL8812AU (TP-Link)"),
        (0x2357, 0x0101, "RTL8812AU (TP-Link alt)"),
    ];
    for (vid, pid, label) in cases {
        assert_eq!(
            wfb_compatible_label(*vid, *pid),
            Some(*label),
            "table miss for {vid:#06x}:{pid:#06x}"
        );
    }
    // A PID outside the table is not known.
    assert_eq!(wfb_compatible_label(0x0BDA, 0x0000), None);
    // The old wrong 0x8812 EU label is gone — 0x8812 is the AU rebadge.
    assert_eq!(wfb_compatible_label(0x0BDA, 0x8812), Some("RTL8812AU"));
}

#[test]
fn build_adapter_info_rtl_eu_driver_is_compatible() {
    let a = build_adapter_info(
        "wlan1".to_string(),
        "rtl88x2eu".to_string(),
        Some((0x0BDA, 0xB812)),
        "phy0".to_string(),
        vec!["managed".to_string(), "monitor".to_string()],
        true,
        Some("managed".to_string()),
    );
    assert!(a.is_wfb_compatible);
    assert_eq!(a.chipset, "RTL8812EU");
    assert_eq!(a.usb_vid, Some(0x0BDA));
    assert_eq!(a.usb_pid, Some(0xB812));
    assert!(a.supports_monitor);
}

#[test]
fn build_adapter_info_a81a_promoted_to_eu_by_driver() {
    // The ambiguous A81A PID becomes the EU label when the EU driver bound.
    let a = build_adapter_info(
        "wlan1".to_string(),
        "rtl88x2eu".to_string(),
        Some((0x0BDA, 0xA81A)),
        "phy0".to_string(),
        vec!["monitor".to_string()],
        true,
        None,
    );
    assert!(a.is_wfb_compatible);
    assert_eq!(a.chipset, "RTL8812EU (a81a)");
    // With a non-EU driver it keeps the default AU rebadge label.
    let b = build_adapter_info(
        "wlan2".to_string(),
        "88XXau".to_string(),
        Some((0x0BDA, 0xA81A)),
        "phy1".to_string(),
        vec!["monitor".to_string()],
        true,
        None,
    );
    assert_eq!(b.chipset, "RTL8812AU (a81a)");
}

#[test]
fn build_adapter_info_denies_aic8800() {
    // The management WiFi can never be flagged compatible, by VID or driver.
    let a = build_adapter_info(
        "wlan0".to_string(),
        "aic8800_fdrv".to_string(),
        Some((0xA69C, 0x8800)),
        "phy0".to_string(),
        vec!["managed".to_string(), "monitor".to_string()],
        true,
        Some("managed".to_string()),
    );
    assert!(!a.is_wfb_compatible);
    // Driver-only deny (no USB ID resolved) still denies.
    let b = build_adapter_info(
        "wlan0".to_string(),
        "brcmfmac".to_string(),
        None,
        String::new(),
        vec![],
        false,
        None,
    );
    assert!(!b.is_wfb_compatible);
}

#[test]
fn build_adapter_info_driver_fallback_when_usb_id_missing() {
    // A USB hub layout can hide the parent IDs; the known DKMS driver name
    // is enough to flag compatibility.
    let a = build_adapter_info(
        "wlan1".to_string(),
        "8812eu".to_string(),
        None,
        "phy0".to_string(),
        vec!["monitor".to_string()],
        true,
        None,
    );
    assert!(a.is_wfb_compatible);
    assert_eq!(a.usb_vid, None);
    assert_eq!(a.usb_pid, None);
}

#[test]
fn build_adapter_info_unknown_is_not_compatible() {
    let a = build_adapter_info(
        "wlan3".to_string(),
        "some_other_wifi".to_string(),
        Some((0x1234, 0x5678)),
        "phy0".to_string(),
        vec!["managed".to_string()],
        false,
        Some("managed".to_string()),
    );
    assert!(!a.is_wfb_compatible);
    assert_eq!(a.chipset, "some_other_wifi");
}

// ── WifiAdapterInfo JSON serialization (the adapter contract wire shape) ──

#[test]
fn wifi_adapter_info_serializes_with_python_field_names() {
    let info = WifiAdapterInfo {
        interface_name: "wlan1".to_string(),
        driver: "rtl88x2eu".to_string(),
        chipset: "RTL8812EU".to_string(),
        supports_monitor: true,
        current_mode: Some("monitor".to_string()),
        phy: "phy0".to_string(),
        usb_vid: Some(0x0BDA),
        usb_pid: Some(0xB812),
        usb_speed_mbps: Some(480),
        is_wfb_compatible: true,
        capabilities: vec!["managed".to_string(), "monitor".to_string()],
    };
    let v = serde_json::to_value(&info).unwrap();
    assert_eq!(v["interface_name"], "wlan1");
    assert_eq!(v["driver"], "rtl88x2eu");
    assert_eq!(v["chipset"], "RTL8812EU");
    assert_eq!(v["supports_monitor"], true);
    assert_eq!(v["current_mode"], "monitor");
    assert_eq!(v["phy"], "phy0");
    assert_eq!(v["usb_vid"], 0x0BDA);
    assert_eq!(v["usb_pid"], 0xB812);
    assert_eq!(v["usb_speed_mbps"], 480);
    assert_eq!(v["is_wfb_compatible"], true);
    assert_eq!(v["capabilities"], serde_json::json!(["managed", "monitor"]));
    // The contract carries the adapter facts the panel + REST read.
    assert_eq!(v.as_object().unwrap().len(), 11);
}

#[test]
fn wifi_adapter_info_unbacked_iface_nulls_mode_and_usb() {
    let info = WifiAdapterInfo {
        interface_name: "wlan9".to_string(),
        driver: "unknown".to_string(),
        chipset: "unknown".to_string(),
        supports_monitor: false,
        current_mode: None,
        phy: String::new(),
        usb_vid: None,
        usb_pid: None,
        usb_speed_mbps: None,
        is_wfb_compatible: false,
        capabilities: vec![],
    };
    let v = serde_json::to_value(&info).unwrap();
    // Missing facts are JSON null, not omitted, so the panel can distinguish
    // "not USB-backed" from "unread".
    assert!(v["current_mode"].is_null());
    assert!(v["usb_vid"].is_null());
    assert!(v["usb_pid"].is_null());
}
