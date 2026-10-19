use super::*;

mod put;

fn leg(id: &str, source: &str) -> CameraLeg {
    CameraLeg {
        id: id.to_string(),
        source: source.to_string(),
        role: None,
        codec: "h264".to_string(),
        width: 1280,
        height: 720,
        fps: 30,
        bitrate_kbps: 4000,
        name: None,
        orientation: None,
        purpose: Vec::new(),
        enabled: true,
        owner: None,
        fov_deg: None,
        mount_pitch_deg: None,
        calibration: None,
        camera_match: None,
        rotation: 0,
        hflip: false,
        vflip: false,
        encoder: "auto".to_string(),
        keyframe_interval: 0,
        capture: false,
        format: Default::default(),
    }
}

fn discovered(name: &str, cam_type: &str, path: &str, m: Option<CameraMatch>) -> DiscoveredCamera {
    DiscoveredCamera {
        name: name.to_string(),
        cam_type: cam_type.to_string(),
        device_path: path.to_string(),
        width: 0,
        height: 0,
        camera_match: m,
    }
}

fn usb_fp(usb: &str) -> CameraMatch {
    CameraMatch {
        usb: Some(usb.to_string()),
        csi_sensor: None,
        csi_port: None,
    }
}

#[test]
fn absent_declared_and_discovered_yields_empty_roster() {
    let rows = build_roster(&[], &[], &HashMap::new());
    assert!(rows.is_empty());
}

#[test]
fn declared_legs_empty_when_no_camera_declared_and_none_discovered() {
    // A camera-less node (no cameras[], no video.camera block, no
    // discovered device) declares no leg — the roster is empty, not a phantom
    // offline `main`.
    let cfg = RosterVideoConfig::default();
    assert!(declared_legs(&cfg, false).is_empty());
}

#[test]
fn declared_legs_synthesizes_main_when_a_device_was_discovered() {
    // A real single-camera drone that relies on the config defaults (no
    // explicit block) still shows its camera, because a device was discovered.
    let cfg = RosterVideoConfig::default();
    let legs = declared_legs(&cfg, true);
    assert_eq!(legs.len(), 1);
    assert_eq!(legs[0].id, "main");
    assert_eq!(legs[0].role.as_deref(), Some("primary"));
}

#[test]
fn declared_legs_synthesizes_main_when_the_camera_block_was_declared() {
    // An explicitly declared camera block shows its `main` leg even when the
    // device is currently absent (it resolves to offline, honestly).
    let cfg = RosterVideoConfig {
        camera: Some(ados_video::config::CameraConfig {
            source: "/dev/video3".to_string(),
            ..Default::default()
        }),
        ..RosterVideoConfig::default()
    };
    let legs = declared_legs(&cfg, false);
    assert_eq!(legs.len(), 1);
    assert_eq!(legs[0].source, "/dev/video3");
}

#[test]
fn declared_legs_returns_the_explicit_cameras_list() {
    let cfg = RosterVideoConfig {
        cameras: vec![leg("eo", "/dev/video0"), leg("ir", "rtsp://pod/ir")],
        ..RosterVideoConfig::default()
    };
    let legs = declared_legs(&cfg, false);
    assert_eq!(legs.len(), 2);
    assert_eq!(legs[0].id, "eo");
}

#[test]
fn declared_local_leg_matched_by_device_path_is_assigned() {
    let mut l = leg("belly", "/dev/video2");
    l.role = Some("primary".to_string());
    let disc = vec![discovered(
        "USB Cam",
        "usb",
        "/dev/video2",
        Some(usb_fp("046d:0825")),
    )];
    let mut live = HashMap::new();
    live.insert("main".to_string(), Some(true));
    let rows = build_roster(&[l], &disc, &live);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["state"], "assigned");
    assert_eq!(rows[0]["device_path"], "/dev/video2");
    assert_eq!(rows[0]["live"], true);
    // The declared leg had no fingerprint → enriched from the discovered one.
    assert_eq!(rows[0]["match"]["usb"], "046d:0825");
}

#[test]
fn declared_local_leg_repins_by_fingerprint_when_the_node_moved() {
    // The leg was declared on /dev/video0 but the camera now enumerates on
    // /dev/video3; the fingerprint re-pins it (still assigned, device re-pinned).
    let mut l = leg("belly", "/dev/video0");
    l.camera_match = Some(usb_fp("046d:0825:ABC"));
    let disc = vec![discovered(
        "USB Cam",
        "usb",
        "/dev/video3",
        Some(usb_fp("046d:0825:ABC")),
    )];
    let rows = build_roster(&[l], &disc, &HashMap::new());
    assert_eq!(rows[0]["state"], "assigned");
    assert_eq!(rows[0]["device_path"], "/dev/video3");
}

#[test]
fn declared_local_leg_absent_device_is_offline() {
    let l = leg("belly", "/dev/video9");
    let rows = build_roster(&[l], &[], &HashMap::new());
    assert_eq!(rows[0]["state"], "offline");
    // The intended configured device path is still surfaced.
    assert_eq!(rows[0]["device_path"], "/dev/video9");
    assert_eq!(rows[0]["live"], Value::Null);
}

#[test]
fn an_offline_leg_is_never_reported_live() {
    // A stale live-stream sidecar still lists the leg as live, but the device
    // is gone (not discovered) → the row is offline AND live is null; the two
    // fields must not contradict each other.
    let l = leg("belly", "/dev/video9");
    let mut live = HashMap::new();
    live.insert("belly".to_string(), Some(true));
    let rows = build_roster(&[l], &[], &live);
    assert_eq!(rows[0]["state"], "offline");
    assert_eq!(rows[0]["live"], Value::Null);
}

#[test]
fn plugin_owned_network_leg_is_plugin_owned() {
    let mut l = leg("ir", "rtsp://192.168.144.25:8554/ir");
    l.owner = Some("com.altnautica.siyi-pod".to_string());
    l.role = Some("ir".to_string());
    let mut live = HashMap::new();
    live.insert("ir".to_string(), Some(true));
    let rows = build_roster(&[l], &[], &live);
    assert_eq!(rows[0]["state"], "plugin_owned");
    assert_eq!(rows[0]["owner"], "com.altnautica.siyi-pod");
    assert_eq!(rows[0]["device_path"], Value::Null);
    assert_eq!(rows[0]["live"], true);
}

#[test]
fn operator_network_leg_is_assigned() {
    let l = leg("cam", "rtsp://10.0.0.9/main");
    let rows = build_roster(&[l], &[], &HashMap::new());
    assert_eq!(rows[0]["state"], "assigned");
}

#[test]
fn tls_network_leg_is_recognised_as_a_network_source() {
    // An `https://` (or `rtsps://`) leg is a network source: it is assigned
    // (present without a discovered device) and carries no device_path.
    for url in ["https://cam.local/stream", "rtsps://10.0.0.9:322/live"] {
        let l = leg("cam", url);
        let rows = build_roster(&[l], &[], &HashMap::new());
        assert_eq!(rows[0]["state"], "assigned", "{url} should be assigned");
        assert_eq!(rows[0]["device_path"], Value::Null);
    }
}

#[test]
fn discovered_device_not_claimed_is_unassigned() {
    let disc = vec![DiscoveredCamera {
        name: "USB Cam".to_string(),
        cam_type: "usb".to_string(),
        device_path: "/dev/video0".to_string(),
        width: 1920,
        height: 1080,
        camera_match: Some(usb_fp("046d:0825")),
    }];
    let rows = build_roster(&[], &disc, &HashMap::new());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["state"], "discovered_unassigned");
    assert_eq!(rows[0]["id"], "video0");
    assert_eq!(rows[0]["enabled"], false);
    assert_eq!(rows[0]["width"], 1920);
    assert_eq!(rows[0]["match"]["usb"], "046d:0825");
}

#[test]
fn legacy_hint_source_matches_a_discovered_device_by_class() {
    // The synthesised legacy `main` leg (source "csi") reconciles to the one
    // discovered CSI camera by device class, so a single-camera drone shows it
    // as the assigned main stream, not an unassigned device.
    let mut l = leg("main", "csi");
    l.role = Some("primary".to_string());
    let disc = vec![discovered(
        "CSI-0 (imx219)",
        "csi",
        "/dev/video0",
        Some(CameraMatch {
            usb: None,
            csi_sensor: Some("imx219".to_string()),
            csi_port: Some(0),
        }),
    )];
    let rows = build_roster(&[l], &disc, &HashMap::new());
    // Exactly one row (the discovered camera was claimed by the main leg).
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "main");
    assert_eq!(rows[0]["state"], "assigned");
    assert_eq!(rows[0]["device_path"], "/dev/video0");
    assert_eq!(rows[0]["match"]["csi_sensor"], "imx219");
}

#[test]
fn hint_source_falls_back_through_the_auto_assign_order() {
    // A `csi`-hint main leg on a node whose only camera is USB reconciles to
    // that USB device (the pipeline's CSI→USB→IP auto-assign fallback), so it
    // is assigned — a single row, not offline + a phantom unassigned USB row.
    let mut l = leg("main", "csi");
    l.role = Some("primary".to_string());
    let disc = vec![discovered(
        "USB Cam",
        "usb",
        "/dev/video0",
        Some(usb_fp("046d:0825")),
    )];
    let rows = build_roster(&[l], &disc, &HashMap::new());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "main");
    assert_eq!(rows[0]["state"], "assigned");
    assert_eq!(rows[0]["device_path"], "/dev/video0");
}

#[test]
fn hint_source_prefers_the_exact_class_before_falling_back() {
    // A `usb` hint on a node with both a CSI and a USB camera takes the USB
    // one (exact-class match wins), leaving the CSI camera as an unassigned
    // candidate row.
    let mut l = leg("main", "usb");
    l.role = Some("primary".to_string());
    let disc = vec![
        discovered("CSI-0 (imx219)", "csi", "/dev/video0", None),
        discovered("USB Cam", "usb", "/dev/video1", None),
    ];
    let rows = build_roster(&[l], &disc, &HashMap::new());
    let main = rows.iter().find(|r| r["id"] == "main").unwrap();
    assert_eq!(main["state"], "assigned");
    assert_eq!(main["device_path"], "/dev/video1");
    assert!(rows
        .iter()
        .any(|r| r["state"] == "discovered_unassigned" && r["source"] == "/dev/video0"));
}

#[test]
fn two_declared_legs_do_not_both_claim_one_device() {
    let a = leg("a", "/dev/video0");
    let b = leg("b", "/dev/video0");
    let disc = vec![discovered("Cam", "usb", "/dev/video0", None)];
    let rows = build_roster(&[a, b], &disc, &HashMap::new());
    // First leg claims the device (assigned); the second finds nothing (offline).
    assert_eq!(rows[0]["state"], "assigned");
    assert_eq!(rows[1]["state"], "offline");
}

#[test]
fn empty_fingerprint_serializes_as_null() {
    let mut l = leg("cam", "/dev/video0");
    l.camera_match = Some(CameraMatch::default());
    let rows = build_roster(&[l], &[], &HashMap::new());
    assert_eq!(rows[0]["match"], Value::Null);
}

#[test]
fn management_fields_pass_through_to_the_row() {
    let mut l = leg("belly", "/dev/video2");
    l.name = Some("Belly cam".to_string());
    l.orientation = Some("down".to_string());
    l.purpose = vec!["detect".to_string(), "precision-landing".to_string()];
    l.enabled = false;
    l.owner = Some("operator".to_string());
    l.fov_deg = Some(82.5);
    l.mount_pitch_deg = Some(-45.0);
    let disc = vec![discovered("Cam", "usb", "/dev/video2", None)];
    let rows = build_roster(&[l], &disc, &HashMap::new());
    let r = &rows[0];
    assert_eq!(r["name"], "Belly cam");
    assert_eq!(r["orientation"], "down");
    assert_eq!(r["purpose"], json!(["detect", "precision-landing"]));
    assert_eq!(r["enabled"], false);
    assert_eq!(r["owner"], "operator");
    assert_eq!(r["fov_deg"], 82.5);
    assert_eq!(r["mount_pitch_deg"], -45.0);
    // owner "operator" is not a plugin → assigned, not plugin_owned.
    assert_eq!(r["state"], "assigned");
}

#[test]
fn bitrate_and_calibration_round_trip_to_the_row() {
    // The roster must carry the leg's transmit bitrate and calibration
    // reference so an operator PUT reads them back and does not silently reset
    // the WFB bitrate to the compiled default.
    let mut l = leg("belly", "/dev/video2");
    l.bitrate_kbps = 6500;
    l.calibration = Some("belly-v1".to_string());
    let disc = vec![discovered("Cam", "usb", "/dev/video2", None)];
    let rows = build_roster(&[l], &disc, &HashMap::new());
    assert_eq!(rows[0]["bitrate_kbps"], 6500);
    assert_eq!(rows[0]["calibration"], "belly-v1");
    // An unassigned discovered row carries neither (it is not a configured leg).
    let disc2 = vec![discovered("Cam", "usb", "/dev/video5", None)];
    let unassigned = build_roster(&[], &disc2, &HashMap::new());
    assert_eq!(unassigned[0]["bitrate_kbps"], Value::Null);
    assert_eq!(unassigned[0]["calibration"], Value::Null);
}

// --- sidecar loaders ---

#[test]
fn load_discovered_tolerates_absent_and_garbage() {
    let dir = tempfile::tempdir().unwrap();
    assert!(load_discovered(&dir.path().join("absent.json")).is_empty());
    let bad = dir.path().join("bad.json");
    std::fs::write(&bad, "not json").unwrap();
    assert!(load_discovered(&bad).is_empty());
    let ok = dir.path().join("ok.json");
    std::fs::write(
            &ok,
            r#"{"version":1,"cameras":[{"name":"c","type":"usb","device_path":"/dev/video0","match":{"usb":"1:2"}}]}"#,
        )
        .unwrap();
    let cams = load_discovered(&ok);
    assert_eq!(cams.len(), 1);
    assert_eq!(cams[0].device_path, "/dev/video0");
    assert_eq!(
        cams[0].camera_match.as_ref().unwrap().usb.as_deref(),
        Some("1:2")
    );
}

#[test]
fn load_live_maps_id_to_liveness() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("video-streams.json");
    std::fs::write(
            &path,
            r#"{"version":2,"streams":[{"id":"main","role":"eo","codec":"h265","live":true},{"id":"ir","role":"ir","codec":"h264","live":false}]}"#,
        )
        .unwrap();
    let live = load_live(&path);
    assert_eq!(live.get("main").copied().flatten(), Some(true));
    assert_eq!(live.get("ir").copied().flatten(), Some(false));
    // Absent file → empty map.
    assert!(load_live(&dir.path().join("absent.json")).is_empty());
}
