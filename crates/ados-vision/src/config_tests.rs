use super::*;

fn write_tmp(yaml: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    std::fs::write(&path, yaml).unwrap();
    (dir, path)
}

#[test]
fn missing_file_is_disabled_default() {
    let c = VisionConfig::load_from(std::path::Path::new("/nonexistent/config.yaml"));
    assert!(!c.enabled);
    assert!(c.profile.is_none());
    assert_eq!(c.downscale_width, 640);
    assert_eq!(c.downscale_height, 480);
    assert_eq!(c.slot_count, 4);
    assert_eq!(c.backend, "auto");
    assert_eq!(c.vision_socket_path(), "/run/ados/vision.sock");
}

#[test]
fn no_vision_block_keeps_profile_but_disabled() {
    let yaml = "agent:\n  profile: drone\n";
    let (_d, p) = write_tmp(yaml);
    let c = VisionConfig::load_from(&p);
    assert!(!c.enabled);
    assert_eq!(c.profile.as_deref(), Some("drone"));
    assert!(!c.is_ground_station());
}

#[test]
fn full_vision_block_loads() {
    let yaml = "\
agent:
  profile: drone
vision:
  enabled: true
  socket_dir: /tmp/run
  downscale_width: 1280
  downscale_height: 720
  slot_count: 6
  backend: rknn
simulation:
  enabled: true
video:
  vision:
    enabled: true
cameras:
  - id: main
    source: /dev/video0
  - id: cam_fl
    source: rtsp://192.168.1.50:8554/we_v0_cam_fl
    format: gray8
    capture: true
    trigger: hub
";
    let (_d, p) = write_tmp(yaml);
    let c = VisionConfig::load_from(&p);
    assert!(c.enabled);
    assert_eq!(c.profile.as_deref(), Some("drone"));
    assert_eq!(c.socket_dir, "/tmp/run");
    assert_eq!(c.downscale_width, 1280);
    assert_eq!(c.slot_count, 6);
    assert_eq!(c.backend, "rknn");
    // Only the captured entry is the engine's; its trigger channel is its
    // list index.
    assert_eq!(c.cameras.len(), 1);
    assert_eq!(c.cameras[0].id, "cam_fl");
    assert_eq!(c.cameras[0].trigger_channel, Some(1));
    assert_eq!(
        c.tap_sink.as_deref(),
        Some("/run/ados/vision-tap-main.sock")
    );
    assert!(c.simulation);
    assert_eq!(c.vision_socket_path(), "/tmp/run/vision.sock");
    assert_eq!(c.rknn_socket_path(), "/tmp/run/vision-rknn.sock");
    assert_eq!(
        c.detections_socket_path(),
        "/tmp/run/vision-detections.sock"
    );
    assert_eq!(c.frames_socket_path(), "/tmp/run/vision-frames.sock");
}

#[test]
fn tracker_and_reid_default_off_and_parse() {
    // A config with no tracker/reid keys leaves both off and the model ids
    // unset — the engine publishes raw detections exactly as before.
    let c = VisionConfig::default();
    assert!(!c.tracker_enabled);
    assert!(!c.reid_enabled);
    assert!(c.reid_model_id.is_none());
    assert!(c.designate_camera.is_none());

    let yaml = "\
vision:
  enabled: true
  tracker_enabled: true
  reid_enabled: true
  reid_model_id: com.example.reid-osnet
  designate_camera: uvc-0
";
    let (_d, p) = write_tmp(yaml);
    let c = VisionConfig::load_from(&p);
    assert!(c.tracker_enabled);
    assert!(c.reid_enabled);
    assert_eq!(c.reid_model_id.as_deref(), Some("com.example.reid-osnet"));
    assert_eq!(c.designate_camera.as_deref(), Some("uvc-0"));

    // A vision block that omits the new keys keeps them at the safe default.
    let yaml2 = "vision:\n  enabled: true\n";
    let (_d2, p2) = write_tmp(yaml2);
    let c2 = VisionConfig::load_from(&p2);
    assert!(c2.enabled);
    assert!(!c2.tracker_enabled);
    assert!(!c2.reid_enabled);
}

#[test]
fn detector_absent_by_default_and_parses_to_metadata() {
    use ados_protocol::framebus::{DetectionHead, FrameFormat, ModelExecution, ModelKind};

    // No detector key ⇒ the engine drives no inference of its own.
    assert!(VisionConfig::default().detector.is_none());

    let yaml = "\
vision:
  enabled: true
  detector:
    model_id: com.example.coco-yolov8n
    model_path: /var/ados/models/coco_yolov8n_640_int8.rknn
    class_labels: [person, bicycle, car]
";
    let (_d, p) = write_tmp(yaml);
    let c = VisionConfig::load_from(&p);
    let det = c.detector.expect("detector parsed");
    assert_eq!(det.model_id, "com.example.coco-yolov8n");
    // input dims default to 640 when the YAML omits them.
    assert_eq!(det.input_width, 640);
    assert_eq!(det.input_height, 640);

    let meta = det.to_metadata();
    assert_eq!(meta.id, "com.example.coco-yolov8n");
    assert!(matches!(meta.kind, ModelKind::Detection));
    assert!(matches!(meta.execution, ModelExecution::EngineRun));
    assert!(matches!(meta.input_format, FrameFormat::Rgb24));
    assert!(matches!(meta.head, DetectionHead::Yolo8));
    assert_eq!(meta.input_width, 640);
    assert_eq!(
        meta.model_path.as_deref(),
        Some("/var/ados/models/coco_yolov8n_640_int8.rknn")
    );
    assert_eq!(meta.output_classes, vec!["person", "bicycle", "car"]);

    // A vision block without a detector keeps it absent.
    let (_d2, p2) = write_tmp("vision:\n  enabled: true\n");
    assert!(VisionConfig::load_from(&p2).detector.is_none());
}

#[test]
fn reid_absent_by_default_and_parses_to_portrait_metadata() {
    use ados_protocol::framebus::{FrameFormat, ModelExecution, ModelKind};

    // No reid key ⇒ the tracker stays motion-only.
    assert!(VisionConfig::default().reid.is_none());

    let yaml = "\
vision:
  enabled: true
  tracker_enabled: true
  reid_enabled: true
  reid_model_id: com.example.reid-osnet
  reid:
    model_id: com.example.reid-osnet
    model_path: /var/ados/models/osnet_x0_5_reid.rknn
";
    let (_d, p) = write_tmp(yaml);
    let c = VisionConfig::load_from(&p);
    assert!(c.reid_enabled);
    let reid = c.reid.expect("reid parsed");
    assert_eq!(reid.model_id, "com.example.reid-osnet");
    // OSNet input defaults: 128 wide x 256 tall (portrait, person-shaped).
    assert_eq!(reid.input_width, 128);
    assert_eq!(reid.input_height, 256);

    let meta = reid.to_metadata();
    assert_eq!(meta.id, "com.example.reid-osnet");
    assert!(matches!(meta.kind, ModelKind::Tracking));
    assert!(matches!(meta.execution, ModelExecution::EngineRun));
    assert!(matches!(meta.input_format, FrameFormat::Rgb24));
    assert_eq!(meta.input_width, 128);
    assert_eq!(meta.input_height, 256);
    assert!(meta.output_classes.is_empty(), "an embedder has no classes");
    assert_eq!(
        meta.model_path.as_deref(),
        Some("/var/ados/models/osnet_x0_5_reid.rknn")
    );
}

#[test]
fn ground_station_gate() {
    let yaml = "agent:\n  profile: ground_station\nvision:\n  enabled: true\n";
    let (_d, p) = write_tmp(yaml);
    assert!(VisionConfig::load_from(&p).is_ground_station());
    let yaml2 = "agent:\n  profile: ground-station\nvision:\n  enabled: true\n";
    let (_d2, p2) = write_tmp(yaml2);
    assert!(VisionConfig::load_from(&p2).is_ground_station());
}

#[test]
fn vision_profile_overrides_agent_profile() {
    let yaml = "\
agent:
  profile: drone
vision:
  enabled: true
  profile: ground_station
";
    let (_d, p) = write_tmp(yaml);
    assert!(VisionConfig::load_from(&p).is_ground_station());
}

#[test]
fn malformed_yaml_falls_back_to_default() {
    let yaml = "this: : not [valid yaml }}}";
    let (_d, p) = write_tmp(yaml);
    let c = VisionConfig::load_from(&p);
    assert!(!c.enabled);
}

#[test]
fn effective_slot_count_clamps_to_the_header_range() {
    // The default is in range and unchanged.
    let c = VisionConfig::default();
    assert_eq!(c.effective_slot_count(), default_slot_count());

    // A misconfigured huge depth clamps to the header maximum instead of
    // silently truncating the writer/reader slot math.
    let huge = VisionConfig {
        slot_count: 100_000,
        ..VisionConfig::default()
    };
    assert_eq!(
        huge.effective_slot_count(),
        ados_protocol::framebus::MAX_SLOT_COUNT
    );

    // A too-small depth is raised to the two-slot floor.
    let tiny = VisionConfig {
        slot_count: 0,
        ..VisionConfig::default()
    };
    assert_eq!(tiny.effective_slot_count(), 2);
}

/// The simulated capture rig's config has no `vision:` block: the engine
/// still runs, because it owns the captured cameras.
#[test]
fn captured_cameras_run_the_engine_without_a_vision_block() {
    let (_d, p) = write_tmp(include_str!(
        "../../ados-config/tests/fixtures/sim-capture-config.yaml"
    ));
    let c = VisionConfig::load_from(&p);
    assert!(!c.enabled);
    assert!(c.should_run());
    assert!(c.simulation);
    let ids: Vec<&str> = c.cameras.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["cam_fl", "cam_fr"]);
    assert!(c.tap_sink.is_none());
    // Nothing captured and vision off: idle.
    let (_d2, p2) = write_tmp("agent:\n  profile: drone\n");
    assert!(!VisionConfig::load_from(&p2).should_run());
}

/// One invalid captured entry does not take the other cameras down; it is
/// kept aside for the status sidecar.
#[test]
fn an_invalid_entry_is_reported_and_the_rest_run() {
    let (_d, p) = write_tmp(
        "cameras:\n  - { id: cam_fl, source: /dev/video0, capture: true }\n  - { id: cam_fr, source: /dev/video2, capture: true, trigger: sometimes }\n",
    );
    let c = VisionConfig::load_from(&p);
    let ids: Vec<&str> = c.cameras.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["cam_fl"]);
    assert_eq!(c.invalid_cameras.len(), 1);
    assert_eq!(c.invalid_cameras[0].id, "cam_fr");
    assert_eq!(c.invalid_cameras[0].index, 1);
}
