use super::*;
use std::fs;

#[test]
fn usb_node_name_classification() {
    assert!(is_usb_node_name("1-1"));
    assert!(is_usb_node_name("2-1.4.3"));
    assert!(is_usb_node_name("usb1"));
    assert!(is_usb_node_name("usb2"));
    // The interface node (colon) and the controller (letters) are not nodes.
    assert!(!is_usb_node_name("1-1:1.0"));
    assert!(!is_usb_node_name("xhci-hcd.0.auto"));
    assert!(!is_usb_node_name("0000:00:14.0"));
    assert!(!is_usb_node_name("platform"));
    assert!(!is_usb_node_name(""));
}

fn topo(bind: &str, ancestors: &[&str]) -> UsbTopo {
    UsbTopo {
        bind_id: bind.to_string(),
        ancestors: ancestors.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn guard_no_route_is_fail_closed() {
    let t = topo("1-1", &["usb1"]);
    assert_eq!(
        guard_verdict(&t, &ControlPath::NoRoute),
        GuardVerdict::BlockUnprovable
    );
}

#[test]
fn guard_non_usb_control_allows() {
    let t = topo("1-1", &["usb1"]);
    assert_eq!(guard_verdict(&t, &ControlPath::NonUsb), GuardVerdict::Allow);
}

#[test]
fn guard_blocks_same_device() {
    let t = topo("1-1", &["usb1"]);
    let c = ControlPath::Usb(topo("1-1", &["usb1"]));
    assert_eq!(guard_verdict(&t, &c), GuardVerdict::BlockIsControl);
}

#[test]
fn guard_blocks_control_under_target_hub() {
    // Target is the hub "1-1"; the control device "1-1.4" hangs off it.
    let target = topo("1-1", &["usb1"]);
    let control = ControlPath::Usb(topo("1-1.4", &["1-1", "usb1"]));
    assert_eq!(
        guard_verdict(&target, &control),
        GuardVerdict::BlockSharesBranch
    );
}

#[test]
fn guard_allows_disjoint_usb_devices() {
    // The WFB adapter on 1-1 and the control link on 1-2 share only the root
    // hub; a soft unbind of 1-1 cannot touch 1-2 → allowed.
    let target = topo("1-1", &["usb1"]);
    let control = ControlPath::Usb(topo("1-2", &["usb1"]));
    assert_eq!(guard_verdict(&target, &control), GuardVerdict::Allow);
    // Control on a different controller is also fine.
    let control2 = ControlPath::Usb(topo("2-1", &["usb2"]));
    assert_eq!(guard_verdict(&target, &control2), GuardVerdict::Allow);
}

#[test]
fn topo_walk_finds_bind_id_and_ancestors() {
    let dir = tempfile::tempdir().unwrap();
    // .../usb1/1-1/{idVendor,idProduct} and the interface node 1-1/1-1:1.0
    let device = dir.path().join("usb1").join("1-1");
    let iface_node = device.join("1-1:1.0");
    fs::create_dir_all(&iface_node).unwrap();
    fs::write(device.join("idVendor"), "0bda\n").unwrap();
    fs::write(device.join("idProduct"), "a81a\n").unwrap();

    let t = topo_from_device_dir(&iface_node).unwrap();
    assert_eq!(t.bind_id, "1-1");
    assert_eq!(t.ancestors, vec!["usb1".to_string()]);
}

#[test]
fn topo_walk_none_when_not_usb() {
    let dir = tempfile::tempdir().unwrap();
    // A device dir with no idVendor/idProduct anywhere in the bounded walk.
    let node = dir.path().join("platform").join("eth0-node");
    fs::create_dir_all(&node).unwrap();
    assert!(topo_from_device_dir(&node).is_none());
}

#[test]
fn hub_and_port_splits_device_nodes() {
    assert_eq!(hub_and_port("1-1.1"), Some(("1-1".to_string(), 1)));
    assert_eq!(hub_and_port("2-1.4.3"), Some(("2-1.4".to_string(), 3)));
    // A root-port device hangs off the root hub "usb<bus>".
    assert_eq!(hub_and_port("1-1"), Some(("usb1".to_string(), 1)));
    assert_eq!(hub_and_port("usb1"), None);
    assert_eq!(hub_and_port(""), None);
}

#[test]
fn port_disable_rel_path() {
    assert_eq!(port_disable_rel("1-1", 1), "1-1-port1/disable");
    assert_eq!(port_disable_rel("2-1.4", 3), "2-1.4-port3/disable");
}

#[test]
fn leaf_safe_when_disjoint_from_protected() {
    // Camera leaf 1-1.1, with RTL 1-1.2 + FC 1-1.4 protected: a leaf unbind
    // of 1-1.1 cannot touch either → safe.
    let cam = topo("1-1.1", &["1-1", "usb1"]);
    let protected = vec![
        topo("1-1.2", &["1-1", "usb1"]),
        topo("1-1.4", &["1-1", "usb1"]),
    ];
    assert!(target_safe_as_leaf(&cam, &protected));
    // But the hub 1-1 is NOT leaf-safe: RTL + FC descend from it.
    let hub = topo("1-1", &["usb1"]);
    assert!(!target_safe_as_leaf(&hub, &protected));
    // Nor is the camera if it somehow IS a protected device.
    assert!(!target_safe_as_leaf(
        &cam,
        &[topo("1-1.1", &["1-1", "usb1"])]
    ));
}

#[test]
fn multi_guard_blocks_hub_reset_that_carries_a_protected_device() {
    // Pi 4B reality: camera + RTL + FC share internal hub 1-1. Resetting the
    // hub would re-enumerate the RTL (1-1.2) → BlockSharesBranch.
    let hub = topo("1-1", &["usb1"]);
    let protected = vec![
        ControlPath::NonUsb,                               // onboard-WiFi mgmt
        ControlPath::Usb(topo("1-1.2", &["1-1", "usb1"])), // RTL
        ControlPath::Usb(topo("1-1.4", &["1-1", "usb1"])), // FC
    ];
    assert_eq!(
        guard_verdict_multi(&hub, &protected),
        GuardVerdict::BlockSharesBranch
    );
}

#[test]
fn multi_guard_allows_isolated_hub_and_failcloses_on_empty() {
    // A camera-only hub 3-1 with no protected device beneath it → allowed.
    let hub = topo("3-1", &["usb3"]);
    let protected = vec![
        ControlPath::NonUsb,
        ControlPath::Usb(topo("1-1.2", &["1-1", "usb1"])),
    ];
    assert_eq!(guard_verdict_multi(&hub, &protected), GuardVerdict::Allow);
    // Empty protected set is fail-closed.
    assert_eq!(
        guard_verdict_multi(&hub, &[]),
        GuardVerdict::BlockUnprovable
    );
    // A NoRoute in the set fails closed.
    assert_eq!(
        guard_verdict_multi(&hub, &[ControlPath::NoRoute]),
        GuardVerdict::BlockUnprovable
    );
}
