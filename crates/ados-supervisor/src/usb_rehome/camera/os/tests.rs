use super::*;

#[test]
fn camera_usb_recovery_sidecar_version_matches_registry() {
    // The per-file const and the sidecar registry are the two sources of
    // truth for this sidecar's schema version; a drift is caught here.
    assert_eq!(
        CAMERA_USB_RECOVERY_SIDECAR_VERSION,
        ados_protocol::contracts::sidecar_version("camera-usb-recovery").unwrap()
    );
}

#[test]
fn name_ancestors_synthesizes_from_bind_id() {
    assert_eq!(
        name_ancestors("1-1.1"),
        vec!["1-1".to_string(), "usb1".to_string()]
    );
    assert_eq!(name_ancestors("1-1"), vec!["usb1".to_string()]);
    assert_eq!(
        name_ancestors("2-1.4.3"),
        vec!["2-1.4".to_string(), "2-1".to_string(), "usb2".to_string()]
    );
}

#[test]
fn each_camera_keeps_its_own_location_record() {
    // The primary keeps the original file, so `auto` expectation survives.
    assert_eq!(last_good_name(true, "front"), "camera-last-good.json");
    assert_eq!(
        last_good_name(false, "belly"),
        "camera-last-good-belly.json"
    );
    // An id never escapes the directory.
    assert_eq!(
        last_good_name(false, "../etc/x"),
        "camera-last-good-___etc_x.json"
    );
}
