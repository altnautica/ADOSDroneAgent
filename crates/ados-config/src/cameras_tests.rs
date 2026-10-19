use super::*;

fn yaml(text: &str) -> Value {
    serde_norway::from_str(text).unwrap()
}

/// The whole config the simulator's fleet VM writes for a capture rig: the
/// same blocks and keys its capture-config script emits (no `vision:` block,
/// no `video.camera.source`), with documentation addresses.
const SIM_RIG: &str = include_str!("../tests/fixtures/sim-capture-config.yaml");

#[test]
fn the_simulated_rig_declares_two_captured_cameras() {
    assert_eq!(
        declared_cameras_from(SIM_RIG),
        DeclaredCameras {
            captured: 2,
            network: false
        }
    );
    let mixed = "cameras:\n  - { id: a, source: rtsp://pod/a }\n  - { id: b, source: /dev/video0, capture: true, enabled: false }\n";
    assert_eq!(
        declared_cameras_from(mixed),
        DeclaredCameras {
            captured: 0,
            network: true
        }
    );
    assert_eq!(declared_cameras_from(""), DeclaredCameras::default());
}

#[test]
fn a_zero_exposure_is_refused() {
    assert!(
        capture_cameras_from("cameras:\n  - { id: a, capture: true, exposure_us: 0 }\n").is_empty()
    );
    let ok = capture_cameras_from("cameras:\n  - { id: a, capture: true, exposure_us: 1 }\n");
    assert_eq!(ok[0].exposure_us, Some(1));
}

#[test]
fn the_simulated_rig_block_parses_with_index_trigger_channels() {
    let cams = capture_cameras_from(SIM_RIG);
    assert_eq!(cams.len(), 2);
    assert_eq!(cams[0].id, "cam_fl");
    assert_eq!(cams[0].role.as_deref(), Some("front_left"));
    assert_eq!(cams[0].source, "rtsp://192.168.1.50:8554/we_v0_cam_fl");
    assert_eq!(
        (cams[0].width, cams[0].height, cams[0].fps),
        (1280, 800, 20)
    );
    assert_eq!(cams[0].format, CaptureFormat::Rgb24);
    assert_eq!(cams[0].trigger, CameraTrigger::Hub);
    assert_eq!(cams[0].trigger_channel, Some(0));
    assert_eq!(cams[1].trigger_channel, Some(1));
}

#[test]
fn only_enabled_captured_entries_are_returned_and_channels_keep_list_order() {
    let text = "cameras:\n  - id: main\n    source: /dev/video0\n  - id: nav\n    source: /dev/video2\n    capture: true\n    format: gray8\n    exposure_us: 4000\n    gain: 2.0\n    trigger: fc\n  - id: off\n    capture: true\n    enabled: false\n  - id: pinned\n    capture: true\n    trigger_channel: 5\n    format: bayer\n";
    let cams = capture_cameras_from(text);
    let ids: Vec<&str> = cams.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["nav", "pinned"]);
    assert_eq!(cams[0].trigger_channel, Some(1));
    assert_eq!(cams[0].format, CaptureFormat::Gray8);
    assert_eq!(cams[0].exposure_us, Some(4000));
    assert_eq!(cams[0].gain, Some(2.0));
    assert_eq!(cams[0].trigger, CameraTrigger::Fc);
    assert_eq!(cams[1].trigger_channel, Some(5));
    assert_eq!(cams[1].format, CaptureFormat::BayerRggb8);
    assert_eq!(cams[1].source, "csi");
}

#[test]
fn a_missing_or_malformed_list_is_empty() {
    assert!(capture_cameras_from("agent:\n  profile: drone\n").is_empty());
    assert!(capture_cameras_from("cameras: 3\n").is_empty());
}

#[test]
fn framebus_sources_round_trip() {
    assert_eq!(framebus_source("cam_fl"), "framebus://cam_fl");
    assert_eq!(framebus_camera_id("framebus://cam_fl"), Some("cam_fl"));
    assert_eq!(framebus_camera_id("framebus://"), None);
    assert_eq!(framebus_camera_id("/dev/video0"), None);
    assert_eq!(framebus_camera_id("rtsp://host/x"), None);
}

#[test]
fn every_capture_format_maps_to_its_frame_format() {
    for (fmt, bytes) in [
        (CaptureFormat::Gray8, 1),
        (CaptureFormat::Gray16, 2),
        (CaptureFormat::Rgb24, 3),
        (CaptureFormat::BayerRggb8, 1),
        (CaptureFormat::BayerRggb16, 2),
    ] {
        assert_eq!(fmt.frame_format().frame_bytes(1, 1), bytes, "{fmt:?}");
    }
    assert_eq!(CaptureFormat::Nv12.frame_format().frame_bytes(2, 2), 6);
    assert_eq!(CaptureFormat::Yuv420p.frame_format().frame_bytes(2, 2), 6);
}

const LEGACY: &str = "video:\n  mode: wfb\n  cameras:\n    - id: main\n      source: /dev/video0\n      role: primary\n    - id: belly\n      source: /dev/video2\nvision:\n  enabled: true\n  cameras:\n    - id: belly\n      kind: capture\n    - id: nav\n      kind: capture\n      device_path: /dev/video4\n    - id: main\n      kind: tap\n";

const MIGRATED: &str = "video:\n  mode: wfb\nvision:\n  enabled: true\ncameras:\n  - id: main\n    source: /dev/video0\n    role: primary\n  - id: belly\n    source: /dev/video2\n    capture: true\n  - id: nav\n    source: /dev/video4\n    capture: true\n";

#[test]
fn the_legacy_lists_migrate_into_the_top_level_list() {
    let mut doc = yaml(LEGACY);
    assert!(migrate_legacy_camera_lists(&mut doc));
    assert_eq!(doc, yaml(MIGRATED));
}

#[test]
fn the_migration_round_trips_through_yaml_and_is_idempotent() {
    let mut doc = yaml(LEGACY);
    assert!(migrate_legacy_camera_lists(&mut doc));
    let text = serde_norway::to_string(&doc).unwrap();
    let mut again = yaml(&text);
    assert!(!migrate_legacy_camera_lists(&mut again));
    assert_eq!(again, yaml(MIGRATED));
    // The migrated file reads back as the captured entries the engine owns.
    let ids: Vec<String> = capture_cameras_from(&text)
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(ids, ["belly", "nav"]);
}

#[test]
fn an_existing_top_level_list_wins_and_only_the_retired_keys_go() {
    let mut doc =
        yaml("cameras:\n  - id: a\nvideo:\n  cameras:\n    - id: b\nvision:\n  cameras: []\n");
    assert!(migrate_legacy_camera_lists(&mut doc));
    assert_eq!(doc, yaml("cameras:\n  - id: a\nvideo: {}\nvision: {}\n"));
}

#[test]
fn a_config_without_retired_keys_is_untouched() {
    for text in [
        "agent:\n  profile: drone\n",
        "cameras:\n  - id: a\n",
        "video:\n  mode: wfb\n",
        "- not a mapping\n",
    ] {
        let mut doc = yaml(text);
        assert!(!migrate_legacy_camera_lists(&mut doc), "{text}");
        assert_eq!(doc, yaml(text));
    }
}

#[test]
fn tap_only_vision_entries_leave_no_list_behind() {
    let mut doc = yaml("vision:\n  cameras:\n    - id: main\n      kind: tap\n");
    assert!(migrate_legacy_camera_lists(&mut doc));
    assert_eq!(doc, yaml("vision: {}\n"));
}

#[test]
fn one_invalid_entry_is_skipped_and_reported_without_taking_down_the_list() {
    let text = "cameras:\n  - { id: a, source: /dev/video0, capture: true }\n  - { id: bad, capture: true, exposure_us: 0 }\n  - { id: worse, width: wide }\n  - { source: /dev/video4, encoder: turbo }\n  - { id: c, source: rtsp://pod/c }\n";
    let e = camera_entries(text);
    let ids: Vec<&str> = e.valid.iter().map(|(_, _, c)| c.id.as_str()).collect();
    assert_eq!(ids, ["a", "c"]);
    // Positions are kept, so trigger channels do not shift.
    assert_eq!(e.valid[1].0, 4);
    assert_eq!(e.valid[1].2.trigger_channel, Some(4));
    let bad: Vec<(usize, &str)> = e.invalid.iter().map(|i| (i.index, i.id.as_str())).collect();
    assert_eq!(bad, [(1, "bad"), (2, "worse"), (3, "cameras[3]")]);
    assert!(e.invalid[0].error.contains("exposure_us"));
    assert_eq!(
        declared_cameras_from(text),
        DeclaredCameras {
            captured: 1,
            network: true
        }
    );
    // Not a list at all: nothing configured, nothing valid.
    assert!(camera_entries("cameras: 3\n").valid.is_empty());
}
