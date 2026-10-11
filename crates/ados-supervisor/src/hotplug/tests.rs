use super::*;

fn usb(vid: u16, pid: u16, devnum: u32) -> hardware::UsbDevice {
    hardware::UsbDevice {
        vid,
        pid,
        bus_dev: format!("1-{devnum}"),
    }
}

#[test]
fn every_supported_adapter_counts_as_a_radio_for_hot_plug() {
    // Every adapter the boot detection accepts must also be one whose
    // replug triggers recovery — including the TP-Link rebadges, which do
    // not carry the Realtek vendor id.
    for (vid, pid, label) in ados_protocol::wfb_tables::WFB_COMPATIBLE {
        assert_eq!(
            radio_ids_in(&[usb(0x1D6B, 0x0002, 1), usb(*vid, *pid, 2)]).len(),
            1,
            "{label} ({vid:#06x}:{pid:#06x}) must count as a radio"
        );
    }
    // A management-WiFi chip and an empty bus are not radios.
    assert!(radio_ids_in(&[usb(0xA69C, 0x8801, 3)]).is_empty());
    assert!(radio_ids_in(&[]).is_empty());
}

#[test]
fn a_radio_replugged_inside_one_poll_reads_as_a_change() {
    let (vid, pid, _) = ados_protocol::wfb_tables::WFB_COMPATIBLE[0];
    let before = radio_ids_in(&[usb(vid, pid, 4)]);
    // Same adapter, re-enumerated: the kernel hands it a new device number.
    let after = radio_ids_in(&[usb(vid, pid, 5)]);
    assert_eq!(before.len(), after.len());
    assert_ne!(before, after);
}

/// Node names for the classification tests. CP2102 / CH340 / Espressif are
/// the RC-bridge ids; STM native USB is the archetypal FC.
fn node(name: &str, ids: Option<(u16, u16)>) -> hardware::SerialNode {
    hardware::SerialNode {
        name: name.to_string(),
        usb: ids.map(|(vid, pid)| usb(vid, pid, 7)),
    }
}

/// Class presence of the classification, which is what these cases pin.
fn classify(nodes: &[hardware::SerialNode], pin_node: &str, mode: &str) -> (bool, bool) {
    let (fc, elrs) = classify_serial_nodes(nodes, pin_node, mode);
    (!fc.is_empty(), !elrs.is_empty())
}

#[test]
fn a_pinned_node_in_crsf_rc_mode_classifies_as_elrs() {
    // The RC lane owns the pinned port in the default mode: the module
    // alone reads (fc=false, elrs=true), never as an FC.
    let nodes = [node("ttyUSB0", Some((0x10C4, 0xEA60)))];
    assert_eq!(classify(&nodes, "ttyUSB0", "crsf_rc"), (false, true));
    // With an FC beside it, both classes are present and independent.
    let both = [
        node("ttyACM0", Some((0x0483, 0x5740))),
        node("ttyUSB0", Some((0x10C4, 0xEA60))),
    ];
    assert_eq!(classify(&both, "ttyUSB0", "crsf_rc"), (true, true));
}

#[test]
fn a_pinned_node_in_mavlink_mode_classifies_as_fc() {
    // In mavlink mode the MAVLink router owns the pinned module as its FC
    // source, so the pinned node counts as FC presence — a replug restarts
    // the FC link (the owner), never the RC lane.
    let nodes = [node("ttyUSB0", Some((0x10C4, 0xEA60)))];
    assert_eq!(classify(&nodes, "ttyUSB0", "mavlink"), (true, false));
    // An FC beside it: both are FC, so ELRS stays absent.
    let both = [
        node("ttyACM0", Some((0x0483, 0x5740))),
        node("ttyUSB0", Some((0x10C4, 0xEA60))),
    ];
    assert_eq!(classify(&both, "ttyUSB0", "mavlink"), (true, false));
}

#[test]
fn a_pinned_node_in_airport_mode_stays_off_the_fc_link() {
    // Only mavlink mode hands the pin to the router; airport leaves it to
    // the RC lane, so the pinned node reads ELRS (never a spurious FC
    // restart, since the router excludes the pin outside mavlink mode).
    let nodes = [node("ttyUSB0", Some((0x10C4, 0xEA60)))];
    assert_eq!(classify(&nodes, "ttyUSB0", "airport"), (false, true));
}

#[test]
fn a_bridge_connected_fc_is_not_stolen_by_a_different_pinned_elrs() {
    // The bridge-VID over-claim is gone: with an ELRS module pinned on one
    // node, an FC behind a CP2102/CH340/ESP32-S3 bridge on ANOTHER node is
    // still FC (not stolen by VID). Only the pinned node is ELRS.
    let nodes = [
        node("ttyUSB0", Some((0x10C4, 0xEA60))), // an FC behind a CP2102 bridge
        node("ttyUSB1", Some((0x1A86, 0x7523))), // the pinned ELRS module
    ];
    assert_eq!(classify(&nodes, "ttyUSB1", "crsf_rc"), (true, true));
}

#[test]
fn an_unpinned_bridge_node_is_an_fc_candidate_in_every_mode() {
    // With no pin, a known RC-bridge node is an FC candidate regardless of
    // mode — a VID:PID cannot distinguish an FC behind the bridge from an
    // RC module behind the same one, and only a pin makes it ELRS.
    for usb in [(0x10C4, 0xEA60), (0x1A86, 0x7523), (0x303A, 0x1001)] {
        for mode in ["crsf_rc", "mavlink", "airport"] {
            let nodes = [node("ttyUSB0", Some(usb))];
            assert_eq!(
                classify(&nodes, "", mode),
                (true, false),
                "usb={usb:?} mode={mode}"
            );
        }
    }
}

#[test]
fn an_fc_replug_reads_fc_presence_so_the_fc_link_restarts() {
    // A real FC on ttyACM0 with the RC module pinned on ttyUSB0 (RC mode):
    // the FC reads fc=true, so its remove/add edge emits DevKind::Fc and
    // restarts ados-mavlink — while the pinned module reads ELRS and never
    // steals that restart.
    let with_fc = [
        node("ttyACM0", Some((0x0483, 0x5740))),
        node("ttyUSB0", Some((0x10C4, 0xEA60))),
    ];
    assert_eq!(classify(&with_fc, "ttyUSB0", "crsf_rc"), (true, true));
    // The FC unplugged: fc drops to false (elrs still present) — a real
    // presence transition on the FC class, which run() turns into a
    // DevKind::Fc restart of the FC link.
    let without_fc = [node("ttyUSB0", Some((0x10C4, 0xEA60)))];
    assert_eq!(classify(&without_fc, "ttyUSB0", "crsf_rc"), (false, true));
}

#[test]
fn pin_matches_only_its_own_node() {
    // A pin on ttyUSB1 leaves ttyUSB0 an FC candidate and reads no ELRS
    // while the pinned node is absent (truthful: not present).
    let nodes = [node("ttyUSB0", None)];
    assert_eq!(classify(&nodes, "ttyUSB1", "crsf_rc"), (true, false));
    // No nodes at all: neither class present.
    assert_eq!(classify(&[], "ttyUSB0", "crsf_rc"), (false, false));
}

#[test]
fn pin_node_name_takes_the_basename_of_an_unresolvable_path() {
    assert_eq!(
        pin_node_name("/dev/nonexistent-tty-xyz"),
        "nonexistent-tty-xyz"
    );
    assert_eq!(pin_node_name("  "), "");
    assert_eq!(pin_node_name(""), "");
}

#[cfg(unix)]
#[test]
fn pin_node_name_resolves_a_symlink_pin_to_its_target_node() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("ttyUSB0");
    std::fs::write(&real, b"").unwrap();
    let link = dir.path().join("usb-elrs-module-if00");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert_eq!(pin_node_name(link.to_str().unwrap()), "ttyUSB0");
}

#[test]
fn crsf_claim_reads_the_radio_section_and_defaults_empty() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    // No mode set → the default crsf_rc (the RC lane owns the pin).
    std::fs::write(
        &cfg,
        "radio:\n  crsf:\n    enabled: true\n    device: \" /dev/ttyUSB0 \"\n",
    )
    .unwrap();
    assert_eq!(
        read_crsf_claim(&cfg),
        CrsfClaim {
            device: "/dev/ttyUSB0".to_string(),
            mode: "crsf_rc".to_string(),
        }
    );
    // An explicit mavlink mode is read through, so the router-owned pin is
    // classified as FC, not ELRS.
    let mav = dir.path().join("mavlink.yaml");
    std::fs::write(
        &mav,
        "radio:\n  crsf:\n    enabled: true\n    device: /dev/ttyUSB0\n    mode: mavlink\n",
    )
    .unwrap();
    assert_eq!(
        read_crsf_claim(&mav),
        CrsfClaim {
            device: "/dev/ttyUSB0".to_string(),
            mode: "mavlink".to_string(),
        }
    );
    // Missing file / missing section / malformed file all read the default
    // claim (no pin, crsf_rc).
    assert_eq!(
        read_crsf_claim(&dir.path().join("nope.yaml")),
        CrsfClaim::default()
    );
    let bare = dir.path().join("bare.yaml");
    std::fs::write(&bare, "agent:\n  profile: drone\n").unwrap();
    assert_eq!(read_crsf_claim(&bare), CrsfClaim::default());
    let bad = dir.path().join("bad.yaml");
    std::fs::write(&bad, ": not yaml [\n").unwrap();
    assert_eq!(read_crsf_claim(&bad), CrsfClaim::default());
    // The config model writes `device: null` for "no pin"; the claim must
    // read it as an unpinned lane at the default mode, not fail the parse.
    let nulled = dir.path().join("nulled.yaml");
    std::fs::write(
        &nulled,
        "radio:\n  crsf:\n    enabled: true\n    device: null\n",
    )
    .unwrap();
    assert_eq!(
        read_crsf_claim(&nulled),
        CrsfClaim {
            device: String::new(),
            mode: "crsf_rc".to_string(),
        }
    );
}

#[test]
fn first_edge_restarts_repeats_within_window_coalesce() {
    let mut c = HotplugCoordinator::with_debounce(Duration::from_secs(3));
    let t0 = Instant::now();

    // First edge for a device class issues a restart.
    assert!(c.should_restart(DevKind::Fc, t0));

    // A re-enumeration edge ~1s later (DFU → flight) is coalesced.
    assert!(!c.should_restart(DevKind::Fc, t0 + Duration::from_secs(1)));
    // And another inside the window.
    assert!(!c.should_restart(DevKind::Fc, t0 + Duration::from_millis(2500)));

    // Past the window, a fresh plug event restarts again.
    assert!(c.should_restart(DevKind::Fc, t0 + Duration::from_millis(3001)));
}

#[test]
fn a_coalesced_edge_leaves_one_trailing_restart_after_the_window() {
    // Remove edge restarts the unit while the device is absent; the add
    // edge 1 s later is coalesced. The device is back, so one more restart
    // must follow once the window has passed.
    let mut c = HotplugCoordinator::with_debounce(Duration::from_secs(3));
    let t0 = Instant::now();
    assert!(c.should_restart(DevKind::Camera, t0));
    assert!(!c.should_restart(DevKind::Camera, t0 + Duration::from_secs(1)));
    assert!(c.due_trailing(t0 + Duration::from_secs(2)).is_empty());
    assert_eq!(
        c.due_trailing(t0 + Duration::from_secs(3)),
        vec![DevKind::Camera]
    );
    // Delivered once.
    assert!(c.due_trailing(t0 + Duration::from_secs(10)).is_empty());
}

#[test]
fn debounce_is_independent_per_device_class() {
    let mut c = HotplugCoordinator::with_debounce(Duration::from_secs(3));
    let t0 = Instant::now();

    // A flight-controller edge does not debounce a camera edge.
    assert!(c.should_restart(DevKind::Fc, t0));
    assert!(c.should_restart(DevKind::Camera, t0));
    assert!(c.should_restart(DevKind::Radio, t0));
    assert!(c.should_restart(DevKind::Elrs, t0));

    // Each class debounces only against its own last restart.
    assert!(!c.should_restart(DevKind::Fc, t0 + Duration::from_secs(1)));
    assert!(!c.should_restart(DevKind::Camera, t0 + Duration::from_secs(1)));
    assert!(!c.should_restart(DevKind::Radio, t0 + Duration::from_secs(1)));
    assert!(!c.should_restart(DevKind::Elrs, t0 + Duration::from_secs(1)));
}

#[test]
fn window_boundary_is_exclusive() {
    let mut c = HotplugCoordinator::with_debounce(Duration::from_secs(3));
    let t0 = Instant::now();
    assert!(c.should_restart(DevKind::Radio, t0));
    // Exactly at the window edge the next edge is allowed (>= window).
    assert!(c.should_restart(DevKind::Radio, t0 + Duration::from_secs(3)));
}
