use super::*;

fn camera(kind: &str, path: &str, role: &str) -> DiscoveredCamera {
    DiscoveredCamera {
        name: format!("{kind} {path}"),
        camera_type: kind.to_string(),
        device_path: path.to_string(),
        width: 0,
        height: 0,
        capabilities: Vec::new(),
        hardware_role: role.to_string(),
    }
}

/// A probe that found a CSI camera on `/dev/video0` (its pick) and USB
/// cameras on `/dev/video2` and `/dev/video4`, plus a codec node.
fn probe() -> DiscoveryResult {
    DiscoveryResult {
        cameras: vec![
            camera("usb", "/dev/video11", "codec"),
            camera("csi", "/dev/video0", "camera"),
            camera("usb", "/dev/video2", "camera"),
            camera("usb", "/dev/video4", "camera"),
        ],
        primary: Some(Primary {
            device_path: "/dev/video0".into(),
            name: "csi /dev/video0".into(),
        }),
        total_cameras: 4,
    }
}

fn primary_path(r: &DiscoveryResult) -> Option<&str> {
    r.primary.as_ref().map(|p| p.device_path.as_str())
}

#[test]
fn the_configured_device_wins_over_the_probe_pick() {
    // cameras: [{id: eo, role: primary, source: /dev/video2},
    //           {id: nav, source: /dev/video0, capture: true}]
    let mut r = probe();
    r.assign_primary("/dev/video2", &["/dev/video0"]);
    assert_eq!(primary_path(&r), Some("/dev/video2"));
    assert_eq!(r.primary.as_ref().unwrap().name, "usb /dev/video2");
}

#[test]
fn a_captured_device_is_never_the_primary() {
    // A hint source: the probe's pick is owned, so the next ranked camera.
    let mut r = probe();
    r.assign_primary("csi", &["/dev/video0"]);
    assert_eq!(primary_path(&r), Some("/dev/video2"));
    // A configured device the vision engine owns is not honoured either.
    let mut r = probe();
    r.assign_primary("/dev/video0", &["/dev/video0", "/dev/video2"]);
    assert_eq!(primary_path(&r), Some("/dev/video4"));
    // Every real camera owned: no primary, never the codec node.
    let mut r = probe();
    r.assign_primary("csi", &["/dev/video0", "/dev/video2", "/dev/video4"]);
    assert_eq!(primary_path(&r), None);
}

#[test]
fn without_a_roster_override_the_probe_pick_stands() {
    let mut r = probe();
    r.assign_primary("csi", &[]);
    assert_eq!(primary_path(&r), Some("/dev/video0"));
    // A configured device the probe did not find leaves the pick as is.
    let mut r = probe();
    r.assign_primary("/dev/video8", &[]);
    assert_eq!(primary_path(&r), Some("/dev/video0"));
    // So does an empty probe.
    let mut r = DiscoveryResult::empty();
    r.assign_primary("/dev/video2", &["/dev/video0"]);
    assert_eq!(primary_path(&r), None);
}
