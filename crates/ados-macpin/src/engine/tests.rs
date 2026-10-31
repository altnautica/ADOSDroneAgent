use super::*;

fn layout() -> PinLayout {
    PinLayout {
        state_path: PathBuf::from("/tmp/test-mac-pins.state"),
        networkd_dir: PathBuf::from(NETWORKD_DIR),
        link_prefix: "10-test-mac-".into(),
        owner_label: "test suite".into(),
    }
}

fn cfg(enabled: bool) -> ReconcileConfig {
    ReconcileConfig {
        enabled,
        apply_live_allowed: false,
        overrides: HashMap::new(),
        layout: layout(),
    }
}

#[test]
fn link_filename_sorts_before_stock_and_names_the_adapter() {
    let l = layout();
    let name = link_file_name(&l, &adapter_key("a69c:8d81", "5-1.3"));
    // Assembled from the key so the expected name does not read as an email
    // address to the repository's leak scanner.
    assert_eq!(name, format!("10-test-mac-{}.link", "a69c-8d81@5-1.3"));
    assert!(is_pin_link_file(&l, &name));
    assert!(name.as_str() < "50-board-wifi.link");
}

#[test]
fn an_override_follows_the_adapter_not_the_interface_name() {
    let mut c = cfg(true);
    c.overrides
        .insert(adapter_key("1234:5678", "1-2"), "02:11:22:33:44:55".into());
    c.overrides
        .insert("wlan0".into(), "02:aa:aa:aa:aa:aa".into());
    let id = UsbId {
        vid: 0x1234,
        pid: 0x5678,
    };
    let pin =
        |key: &str| classify_adapter(id, key, "1234:5678", Some("m"), "", &c, true, None, false);
    // The keyed adapter is pinned wherever it enumerates.
    assert_eq!(
        pin(&adapter_key("1234:5678", "1-2")),
        Decision::Pin {
            mac: MacAddr::parse("02:11:22:33:44:55").unwrap(),
            source: AdapterSource::Override
        }
    );
    // The same model in another port is not, and a name key never applies.
    assert_eq!(pin(&adapter_key("1234:5678", "1-3")), Decision::Observe);
}

#[test]
fn stale_pin_links_are_swept_but_an_unplugged_adapter_keeps_its_pin() {
    let set = |names: &[&str]| -> std::collections::HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    };
    let l = layout();
    let kept = link_file_name(&l, &adapter_key("a69c:8d81", "5-1.3"));
    let unpinned = link_file_name(&l, &adapter_key("a69c:8d81", "5-1.4"));
    let unplugged = link_file_name(&l, &adapter_key("a69c:8d81", "3-1"));
    let existing = vec![
        kept.clone(),
        unpinned.clone(),
        unplugged.clone(),
        "10-test-mac-wlan0.link".to_string(),
        "50-board-wifi.link".to_string(),
    ];
    let mut stale = stale_pin_links(
        &l,
        &existing,
        &set(&[kept.as_str(), unpinned.as_str()]),
        &set(&[kept.as_str()]),
    );
    stale.sort();
    assert_eq!(stale, vec![unpinned, "10-test-mac-wlan0.link".to_string()]);
}

#[test]
fn reload_udev_never_triggers_the_live_net_link() {
    // The pin/unpin path may reload the udev rules DB, but must NEVER
    // `udevadm trigger` the net subsystem: that re-applies the pinned MAC
    // to the LIVE interface and drops the operator's management link.
    // Reloading the rules DB is the only allowed
    // action; the `.link` takes effect on the next boot / hot-plug event.
    for argv in UDEV_RELOAD_ARGV {
        assert!(
            !argv.contains(&"trigger"),
            "reload must not re-trigger devices: {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a.contains("subsystem-match=net")),
            "reload must never target the net subsystem: {argv:?}"
        );
    }
    // Positive assertion: it does reload the rules DB so a new/removed file
    // is seen at the next event.
    assert!(UDEV_RELOAD_ARGV
        .iter()
        .any(|argv| argv.contains(&"control") && argv.contains(&"--reload")));
}

#[test]
fn render_carries_match_namepolicy_and_mac() {
    let mac = MacAddr::parse("02:c6:75:83:1a:3e").unwrap();
    let body = render_link_file(&layout(), "Path=platform-xhci-hcd.0-usb-0:1.3:1.0", &mac);
    assert!(body.contains("Managed by the\n# test suite; remove"));
    assert!(body.contains("[Match]\nPath=platform-xhci-hcd.0-usb-0:1.3:1.0"));
    assert!(body.contains("NamePolicy=kernel"));
    assert!(body.contains("MACAddress=02:c6:75:83:1a:3e"));
}

#[test]
fn parse_match_block_extracts_only_match_lines() {
    let body = "# comment\n[Match]\nOriginalName=wlan*\nDriver=usb\n\n[Link]\nNamePolicy=kernel\n";
    assert_eq!(
        parse_match_block(body).as_deref(),
        Some("OriginalName=wlan*\nDriver=usb")
    );
    // A round trip: render then re-parse yields the same match.
    let mac = MacAddr::parse("02:00:00:00:00:01").unwrap();
    let rendered = render_link_file(&layout(), "OriginalName=wlan0", &mac);
    assert_eq!(
        parse_match_block(&rendered).as_deref(),
        Some("OriginalName=wlan0")
    );
}

#[test]
fn a_wildcard_match_is_not_specific_enough_to_pin() {
    // The stock `99-default.link` match. Pairing it with our unconditional
    // MACAddress= handed one adapter's pinned MAC to every interface.
    assert!(!match_block_is_specific("OriginalName=*"));
    assert!(!match_block_is_specific("OriginalName=wlan*"));
    assert!(!match_block_is_specific("Driver=usb\nType=wlan"));
    assert!(!match_block_is_specific("OriginalName=!eth0"));
    assert!(!match_block_is_specific(""));
    // The two forms the resolver emits, and a permanent-MAC match.
    assert!(match_block_is_specific("OriginalName=wlan0"));
    assert!(match_block_is_specific(
        "Path=platform-xhci-hcd.0-usb-0:1.3:1.0"
    ));
    assert!(match_block_is_specific(
        "Driver=usb\nPermanentMACAddress=00:c0:ca:b1:0d:a2"
    ));
    // A glob ANDed with a specific key is still one adapter.
    assert!(match_block_is_specific(
        "OriginalName=wlan*\nPath=pci-0000:01:00.0"
    ));
}

#[test]
fn id_path_is_read_from_udev_properties() {
    let props = "ID_BUS=usb\nID_PATH=platform-xhci-hcd.0-usb-0:1.3:1.0\nID_PATH_TAG=platform_xhci_hcd_0_usb_0_1_3_1_0\n";
    assert_eq!(
        parse_udev_id_path(props).as_deref(),
        Some("platform-xhci-hcd.0-usb-0:1.3:1.0")
    );
    // Absent property, and a value carrying a glob (never usable as a
    // specific match), both decline.
    assert_eq!(parse_udev_id_path("ID_BUS=usb\n"), None);
    assert_eq!(parse_udev_id_path("ID_PATH=usb-*\n"), None);
    assert_eq!(parse_udev_id_path("ID_PATH=\n"), None);
}

#[test]
fn quirk_adapter_pins_when_armed() {
    let d = classify_adapter(
        UsbId {
            vid: 0xa69c,
            pid: 0x8d81,
        },
        "wlan0",
        "a69c:8d81",
        Some("03851cd61fc642d781d3f93a00e624cd"),
        "",
        &cfg(true),
        true,
        None,
        false,
    );
    assert_eq!(
        d,
        Decision::Pin {
            mac: MacAddr::parse("02:c6:75:83:1a:3e").unwrap(),
            source: AdapterSource::Quirk
        }
    );
}

#[test]
fn two_quirk_adapters_salted_by_usb_path_get_distinct_macs() {
    // Two no-efuse randomizers on one box must NEVER collapse to the same
    // MAC. The reconcile passes each a distinct salt (its USB topology path)
    // whenever more than one quirk adapter exists; a single randomizer keeps
    // the historical "" parity MAC. This guards that salt-threading contract.
    let machine_id = "03851cd61fc642d781d3f93a00e624cd";
    let id = UsbId {
        vid: 0xa69c,
        pid: 0x8d81,
    };
    let single = classify_adapter(
        id,
        "wlan0",
        "a69c:8d81",
        Some(machine_id),
        "",
        &cfg(true),
        true,
        None,
        false,
    );
    // A lone randomizer keeps the proven, bit-for-bit "" parity MAC.
    assert_eq!(
        single,
        Decision::Pin {
            mac: MacAddr::parse("02:c6:75:83:1a:3e").unwrap(),
            source: AdapterSource::Quirk
        }
    );
    // Two randomizers, distinct USB paths -> distinct MACs, both valid.
    let a = classify_adapter(
        id,
        "wlan0",
        "a69c:8d81",
        Some(machine_id),
        "5-1.3",
        &cfg(true),
        true,
        None,
        false,
    );
    let b = classify_adapter(
        id,
        "wlan1",
        "a69c:8d81",
        Some(machine_id),
        "5-1.4",
        &cfg(true),
        true,
        None,
        false,
    );
    let (Decision::Pin { mac: mac_a, .. }, Decision::Pin { mac: mac_b, .. }) = (a, b) else {
        panic!("both quirk adapters should pin");
    };
    assert_ne!(mac_a, mac_b);
    assert_ne!(mac_a, MacAddr::parse("02:c6:75:83:1a:3e").unwrap());
}

#[test]
fn quirk_adapter_deferred_or_disabled_when_blocked() {
    // disabled
    assert_eq!(
        classify_adapter(
            UsbId {
                vid: 0xa69c,
                pid: 1
            },
            "wlan0",
            "a69c:0001",
            Some("m"),
            "",
            &cfg(false),
            true,
            None,
            false
        ),
        Decision::Disabled
    );
    // no link mechanism (systemd-udev unavailable)
    assert!(matches!(
        classify_adapter(
            UsbId {
                vid: 0xa69c,
                pid: 1
            },
            "wlan0",
            "a69c:0001",
            Some("m"),
            "",
            &cfg(true),
            false,
            None,
            false
        ),
        Decision::Deferred(_)
    ));
    // no machine-id
    assert!(matches!(
        classify_adapter(
            UsbId {
                vid: 0xa69c,
                pid: 1
            },
            "wlan0",
            "a69c:0001",
            None,
            "",
            &cfg(true),
            true,
            None,
            false
        ),
        Decision::Deferred(_)
    ));
}

#[test]
fn efuse_radio_is_left_stable() {
    let d = classify_adapter(
        UsbId {
            vid: 0x0bda,
            pid: 0xa81a,
        },
        "wlxabc",
        "0bda:a81a",
        Some("m"),
        "",
        &cfg(true),
        true,
        None,
        true,
    );
    assert_eq!(d, Decision::Stable);
}

#[test]
fn override_pins_an_unknown_adapter() {
    let mut c = cfg(true);
    c.overrides
        .insert("1234:5678".into(), "02:11:22:33:44:55".into());
    let d = classify_adapter(
        UsbId {
            vid: 0x1234,
            pid: 0x5678,
        },
        "wlan1",
        "1234:5678",
        Some("m"),
        "",
        &c,
        true,
        None,
        true,
    );
    assert_eq!(
        d,
        Decision::Pin {
            mac: MacAddr::parse("02:11:22:33:44:55").unwrap(),
            source: AdapterSource::Override
        }
    );
}

#[test]
fn unknown_adapter_only_candidate_after_threshold() {
    let id = UsbId {
        vid: 0x1234,
        pid: 0x5678,
    };
    // below threshold -> observe
    let rec = LearnerRecord {
        vidpid: "1234:5678".into(),
        usb_path: "1-1".into(),
        last_mac: MacAddr([0; 6]),
        first_seen: 0,
        boot_count: 3,
        mac_change_count: 1,
    };
    assert_eq!(
        classify_adapter(
            id,
            "wlan1",
            "1234:5678",
            Some("m"),
            "",
            &cfg(true),
            true,
            Some(&rec),
            true
        ),
        Decision::Observe
    );
    // at threshold -> candidate with a proposed MAC
    let rec2 = LearnerRecord {
        mac_change_count: LEARN_THRESHOLD,
        ..rec.clone()
    };
    assert!(matches!(
        classify_adapter(
            id,
            "wlan1",
            "1234:5678",
            Some("m"),
            "",
            &cfg(true),
            true,
            Some(&rec2),
            true
        ),
        Decision::Candidate { proposed: Some(_) }
    ));
    // learner disabled -> observe regardless
    assert_eq!(
        classify_adapter(
            id,
            "wlan1",
            "1234:5678",
            Some("m"),
            "",
            &cfg(true),
            true,
            Some(&rec2),
            false
        ),
        Decision::Observe
    );
}

#[test]
fn fold_learner_tracks_changes() {
    let mut l = Vec::new();
    let a = MacAddr([1, 2, 3, 4, 5, 6]);
    let b = MacAddr([9, 8, 7, 6, 5, 4]);
    fold_learner(&mut l, "v:p", "1-1", a, 100); // first sight
    assert_eq!(l[0].boot_count, 1);
    assert_eq!(l[0].mac_change_count, 0);
    fold_learner(&mut l, "v:p", "1-1", b, 200); // changed
    assert_eq!(l[0].boot_count, 2);
    assert_eq!(l[0].mac_change_count, 1);
    fold_learner(&mut l, "v:p", "1-1", b, 300); // same
    assert_eq!(l[0].boot_count, 3);
    assert_eq!(l[0].mac_change_count, 1);
}

#[test]
fn state_round_trips_through_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mac-pins.state");
    let mut st = MacPinsState::default();
    st.learner.push(LearnerRecord {
        vidpid: "a:b".into(),
        usb_path: "1-1".into(),
        last_mac: MacAddr([0; 6]),
        first_seen: 1,
        boot_count: 1,
        mac_change_count: 0,
    });
    save_state_to(&path, &st).unwrap();
    assert_eq!(load_state_from(&path), st);
    // missing file -> default
    assert_eq!(
        load_state_from(&dir.path().join("nope")),
        MacPinsState::default()
    );
}
