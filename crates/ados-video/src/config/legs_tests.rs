use super::tests::write_tmp;
use super::*;

// --- cameras[] / resolve_legs -------------------------------------

#[test]
fn cameras_absent_synthesizes_single_main_leg() {
    // No `cameras` ⇒ one primary "main" leg from the legacy block,
    // byte-identical to the single-stream path.
    let cfg = AgentVideoConfig::default();
    let cam = CameraConfig {
        source: "usb".to_string(),
        codec: "h265".to_string(),
        ..CameraConfig::default()
    };
    let legs = cfg.resolve_legs(&cam);
    assert_eq!(legs.len(), 1);
    let leg = &legs[0];
    assert_eq!(leg.id, "main");
    assert_eq!(leg.role, "primary");
    assert_eq!(leg.source, "usb");
    assert_eq!(leg.codec, "h265");
    assert!(leg.is_primary);
    assert!(!leg.is_network_pull);
}

#[test]
fn cameras_parse_multi_leg_and_resolve() {
    let yaml = "\
cameras:
  - { id: eo-zoom, source: rtsp://192.168.144.25:8554/main, role: eo, codec: h265 }
  - { id: eo-wide, source: rtsp://192.168.144.25:8554/sub, role: eo_wide, codec: h264 }
  - { id: ir, source: rtsp://192.168.144.25:8554/ir, role: ir }
";
    let (_dir, path) = write_tmp(yaml);
    let cfg = AgentVideoConfig::load_from(&path);
    assert_eq!(cfg.cameras.len(), 3);
    let legs = cfg.resolve_legs(&CameraConfig::default());
    assert_eq!(legs.len(), 3);
    // No leg declared role "primary" → the first leg is the primary, served
    // at the fixed "main" path as an owned encoder; the two secondary RTSP
    // legs are network pulls that keep their declared ids.
    assert_eq!(legs[0].id, "main");
    assert!(legs[0].is_primary);
    assert!(!legs[0].is_network_pull);
    assert_eq!(legs[0].role, "eo"); // A6: keeps its declared role, not "primary"
    assert_eq!(legs[1].id, "eo-wide");
    assert!(!legs[1].is_primary);
    assert!(legs[1].is_network_pull);
    assert_eq!(legs[1].role, "eo_wide");
    assert!(legs[2].is_network_pull);
    assert_eq!(legs[2].codec, "h264"); // per-leg codec default
}

#[test]
fn resolve_legs_honours_explicit_primary_role() {
    let yaml = "\
cameras:
  - { id: ir, source: rtsp://10.0.0.9/ir, role: ir }
  - { id: main-eo, source: /dev/video0, role: primary }
";
    let (_dir, path) = write_tmp(yaml);
    let legs = AgentVideoConfig::load_from(&path).resolve_legs(&CameraConfig::default());
    // The second leg is the declared primary (owned encoder, served at the
    // fixed "main" path); the first, though listed first, is a secondary
    // network pull that keeps its declared id.
    let primary: Vec<_> = legs.iter().filter(|l| l.is_primary).collect();
    assert_eq!(primary.len(), 1);
    assert_eq!(primary[0].id, "main");
    assert!(!primary[0].is_network_pull);
    assert!(legs.iter().find(|l| l.id == "ir").unwrap().is_network_pull);
}

#[test]
fn resolve_legs_keeps_the_primary_declared_role_label() {
    // A6: the primary leg keeps its declared role (e.g. "eo") so the GCS
    // label map resolves — it is NOT clobbered to "primary". (The SIYI ZT30
    // shape: first leg EO-zoom on main, second IR on sub.)
    let yaml = "\
cameras:
  - { id: eo-zoom, source: rtsp://192.168.144.25:8554/main, role: eo }
  - { id: sub,     source: rtsp://192.168.144.25:8554/sub,  role: ir }
";
    let (_dir, path) = write_tmp(yaml);
    let legs = AgentVideoConfig::load_from(&path).resolve_legs(&CameraConfig::default());
    let primary = legs.iter().find(|l| l.is_primary).unwrap();
    assert_eq!(primary.id, "main"); // served at the fixed main path
    assert_eq!(primary.role, "eo"); // but keeps the EO label
}

#[test]
fn a_disabled_secondary_leg_is_not_served_but_the_primary_always_is() {
    let yaml = "\
cameras:
  - { id: eo, source: /dev/video0, role: primary, enabled: false }
  - { id: belly, source: /dev/video2, enabled: false }
  - { id: ir, source: rtsp://192.168.144.25:8554/ir }
";
    let (_dir, path) = write_tmp(yaml);
    let legs = AgentVideoConfig::load_from(&path).resolve_legs(&CameraConfig::default());
    let ids: Vec<&str> = legs.iter().map(|l| l.id.as_str()).collect();
    // The switched-off secondary gets no encoder, no mediamtx path and no
    // switcher entry; the primary carries the radio leg and always streams.
    assert_eq!(ids, vec!["main", "ir"]);
}

#[test]
fn the_primary_leg_drives_the_primary_encoder_settings() {
    let yaml = "\
video:
  camera:
    source: csi
    width: 1280
    height: 720
    rotation: 0
    thumbnail: { width: 160, height: 90, fps: 1, bitrate_kbps: 30 }
cameras:
  - { id: ir, source: rtsp://192.168.144.25:8554/ir, role: ir }
  - id: eo
    source: rtsp://192.168.144.25:8554/main
    role: primary
    width: 1920
    height: 1080
    fps: 25
    bitrate_kbps: 2500
    rotation: 180
    hflip: true
    encoder: software
    keyframe_interval: 12
";
    let (_dir, path) = write_tmp(yaml);
    let cfg = AgentVideoConfig::load_from(&path);
    let cam = cfg.primary_camera_config(CameraConfig::load_from(&path));
    assert_eq!(cam.source, "rtsp://192.168.144.25:8554/main");
    assert_eq!((cam.width, cam.height, cam.fps), (1920, 1080, 25));
    assert_eq!(cam.bitrate_kbps, 2500);
    assert_eq!(cam.rotation, 180);
    assert!(cam.hflip);
    assert_eq!(cam.encoder, "software");
    assert_eq!(cam.keyframe_interval, 12);
    // The attention thumbnail is a node setting and survives the overlay.
    assert_eq!(cam.thumbnail.width, 160);
}

#[test]
fn without_declared_legs_the_camera_block_is_used_verbatim() {
    let cam = CameraConfig {
        rotation: 90,
        bitrate_kbps: 1800,
        ..CameraConfig::default()
    };
    let out = AgentVideoConfig::default().primary_camera_config(cam.clone());
    assert_eq!(out.rotation, 90);
    assert_eq!(out.bitrate_kbps, 1800);
    assert_eq!(out.source, cam.source);
}

#[test]
fn camera_leg_partial_fields_fill_defaults() {
    let yaml = "\
cameras:
  - { id: solo, source: /dev/video0 }
";
    let (_dir, path) = write_tmp(yaml);
    let cfg = AgentVideoConfig::load_from(&path);
    let leg = &cfg.cameras[0];
    assert_eq!(leg.codec, "h264");
    assert_eq!(leg.width, 1280);
    assert_eq!(leg.height, 720);
    assert_eq!(leg.fps, 30);
    assert_eq!(leg.bitrate_kbps, 4000);
    assert!(leg.role.is_none());
    // The management fields default to the same values the Python `CameraLeg`
    // model does, so a leg declared before the roster fields existed reads the
    // same on both halves (name/orientation/owner/fov/mount/calibration/match
    // absent, purpose empty, enabled true).
    assert!(leg.name.is_none());
    assert!(leg.orientation.is_none());
    assert!(leg.purpose.is_empty());
    assert!(leg.enabled);
    assert!(leg.owner.is_none());
    assert!(leg.fov_deg.is_none());
    assert!(leg.mount_pitch_deg.is_none());
    assert!(leg.calibration.is_none());
    assert!(leg.camera_match.is_none());
    // The encode-plane keys default like the Rust CameraConfig / Python
    // CameraLeg: no image transform, encoder auto, auto keyframe interval.
    assert_eq!(leg.rotation, 0);
    assert!(!leg.hflip);
    assert!(!leg.vflip);
    assert_eq!(leg.encoder, "auto");
    assert_eq!(leg.keyframe_interval, 0);
    // A single declared leg (no role) is the primary owned encoder.
    let legs = cfg.resolve_legs(&CameraConfig::default());
    assert_eq!(legs.len(), 1);
    assert!(legs[0].is_primary);
    assert!(!legs[0].is_network_pull);
}

#[test]
fn camera_leg_management_fields_parse_and_resolve_unchanged() {
    // A leg carrying the full management field set parses every field, and
    // the resolved leg is byte-identical to one without them (the pipeline
    // reads none of the metadata — it stays on id/source/role/codec/geometry).
    let yaml = "\
cameras:
  - id: belly
    source: /dev/video2
    role: primary
    codec: h265
    name: Belly cam
    orientation: down
    purpose: [detect, precision-landing]
    enabled: false
    owner: operator
    fov_deg: 82.5
    mount_pitch_deg: -45.0
    calibration: belly-v1
    match: { usb: \"046d:0825:ABC123\" }
";
    let (_dir, path) = write_tmp(yaml);
    let cfg = AgentVideoConfig::load_from(&path);
    assert_eq!(cfg.cameras.len(), 1);
    let leg = &cfg.cameras[0];
    assert_eq!(leg.name.as_deref(), Some("Belly cam"));
    assert_eq!(leg.orientation.as_deref(), Some("down"));
    assert_eq!(leg.purpose, vec!["detect", "precision-landing"]);
    assert!(!leg.enabled);
    assert_eq!(leg.owner.as_deref(), Some("operator"));
    assert_eq!(leg.fov_deg, Some(82.5));
    assert_eq!(leg.mount_pitch_deg, Some(-45.0));
    assert_eq!(leg.calibration.as_deref(), Some("belly-v1"));
    assert_eq!(
        leg.camera_match.as_ref().and_then(|m| m.usb.as_deref()),
        Some("046d:0825:ABC123")
    );
    // The resolved leg carries only the transport-plane fields; the metadata
    // never reaches the encoder, so an `enabled: false` leg still resolves the
    // same way (enable-gating is a later polish item, not this schema pass).
    let legs = cfg.resolve_legs(&CameraConfig::default());
    assert_eq!(legs.len(), 1);
    assert_eq!(legs[0].id, "main"); // primary → the fixed main path
    assert_eq!(legs[0].role, "primary");
    assert_eq!(legs[0].codec, "h265");
    assert!(legs[0].is_primary);
}

#[test]
fn camera_leg_id_defaults_to_main_matching_python() {
    // A leg written without an explicit `id` resolves to "main" (the legacy
    // single-camera identity), matching the Python CameraLeg.id default so the
    // two halves parse an id-less leg the same way.
    let yaml = "cameras:\n  - { source: /dev/video0 }\n";
    let (_dir, path) = write_tmp(yaml);
    let cfg = AgentVideoConfig::load_from(&path);
    assert_eq!(cfg.cameras.len(), 1);
    assert_eq!(cfg.cameras[0].id, "main");
}

#[test]
fn camera_match_csi_fingerprint_parses() {
    let yaml = "\
cameras:
  - id: nadir
    source: /dev/video0
    match: { csi_sensor: imx219, csi_port: 1 }
";
    let (_dir, path) = write_tmp(yaml);
    let cfg = AgentVideoConfig::load_from(&path);
    let m = cfg.cameras[0].camera_match.as_ref().unwrap();
    assert_eq!(m.csi_sensor.as_deref(), Some("imx219"));
    assert_eq!(m.csi_port, Some(1));
    assert!(m.usb.is_none());
}

// --- encode-plane keys propagate to the encoder --------------------

#[test]
fn camera_leg_encode_plane_keys_reach_to_camera_config() {
    // The image-transform + encoder-selection keys are the ONE set of
    // per-leg keys the pipeline reads, so unlike the roster metadata they
    // must survive `resolve_legs` → `to_camera_config` (the per-leg encoder
    // command is built from that CameraConfig view).
    let yaml = "\
cameras:
  - id: eo
    source: /dev/video0
    role: primary
  - id: belly
    source: /dev/video1
    role: eo_wide
    rotation: 180
    hflip: true
    vflip: false
    encoder: omx
    keyframe_interval: 5
";
    let (_dir, path) = write_tmp(yaml);
    let legs = AgentVideoConfig::load_from(&path).resolve_legs(&CameraConfig::default());
    let belly = legs
        .iter()
        .find(|l| l.id == "belly" && l.role == "eo_wide" && !l.is_primary)
        .unwrap();
    assert_eq!(belly.rotation, 180);
    assert!(belly.hflip);
    assert!(!belly.vflip);
    assert_eq!(belly.encoder, "omx");
    assert_eq!(belly.keyframe_interval, 5);
    let cam = belly.to_camera_config();
    assert_eq!(cam.rotation, 180);
    assert!(cam.hflip);
    assert_eq!(cam.encoder, "omx");
    assert_eq!(cam.keyframe_interval, 5);
}

/// A configured keyframe interval past the GOP ceiling is held at the
/// ceiling on both the single-camera block and a leg; the ceiling itself and
/// the 0 default pass unchanged.
#[test]
fn keyframe_interval_is_held_at_the_gop_ceiling() {
    let yaml = "\
video:
  camera:
    keyframe_interval: 300
cameras:
  - id: eo
    source: /dev/video0
    role: primary
    keyframe_interval: 31
  - id: belly
    source: /dev/video1
    keyframe_interval: 30
";
    let (_dir, path) = write_tmp(yaml);
    assert_eq!(
        CameraConfig::load_from(&path).keyframe_interval,
        MAX_KEYFRAME_INTERVAL
    );
    let cfg = AgentVideoConfig::load_from(&path);
    let by_id = |id: &str| {
        cfg.cameras
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .keyframe_interval
    };
    assert_eq!(by_id("eo"), MAX_KEYFRAME_INTERVAL);
    assert_eq!(by_id("belly"), 30);
    assert_eq!(CameraConfig::default().keyframe_interval, 0);
}

/// A camera the vision engine captures is never opened by the pipeline: its
/// leg (primary or secondary, local device or URL) is encoded from the frame
/// bus at its captured format and geometry.
#[test]
fn a_captured_leg_is_encoded_from_the_frame_bus() {
    let yaml = "\
cameras:
  - id: cam_fl
    role: front_left
    source: rtsp://192.168.1.50:8554/we_v0_cam_fl
    width: 1280
    height: 800
    fps: 20
    format: gray8
    capture: true
    trigger: hub
  - { id: cam_fr, source: /dev/video2, capture: true }
  - { id: belly, source: /dev/video4 }
";
    let (_dir, path) = write_tmp(yaml);
    let cfg = AgentVideoConfig::load_from(&path);
    let legs = cfg.resolve_legs(&CameraConfig::default());
    assert_eq!(legs[0].id, "main");
    assert_eq!(legs[0].role, "front_left");
    assert_eq!(legs[0].source, "framebus://cam_fl");
    assert!(legs[0].is_primary && !legs[0].is_network_pull);
    assert_eq!(
        legs[0].raw_input(),
        Some(crate::encoder::RawInput {
            format: ados_protocol::framebus::FrameFormat::Gray8,
            width: 1280,
            height: 800,
            fps: 20,
        })
    );
    assert_eq!(legs[1].source, "framebus://cam_fr");
    assert!(legs[1].is_owned_encoder());
    assert_eq!(
        legs[1].framebus_format,
        Some(ados_protocol::framebus::FrameFormat::Rgb24)
    );
    assert_eq!(legs[2].source, "/dev/video4");
    assert_eq!(legs[2].raw_input(), None);
    let primary = cfg.primary_camera_config(CameraConfig::default());
    assert_eq!(primary.source, "framebus://cam_fl");
    assert_eq!(primary.framebus_camera(), Some("cam_fl"));
    assert!(primary.network_source().is_none());
}

/// The retired `video.cameras` list is no longer read: the installer moves it
/// to the top-level list, and the services read only that.
#[test]
fn the_retired_video_cameras_list_is_not_read() {
    let (_dir, path) = write_tmp("video:\n  cameras:\n    - { id: eo, source: /dev/video0 }\n");
    assert!(AgentVideoConfig::load_from(&path).cameras.is_empty());
    assert!(RosterVideoConfig::load_from(&path).cameras.is_empty());
}

/// The simulated capture rig's config (no `video.camera` block, no `vision:`
/// block): both cameras are legs the pipeline encodes off the frame bus, the
/// first as the primary.
#[test]
fn the_simulated_rig_is_served_from_the_frame_bus() {
    let (_dir, path) = write_tmp(include_str!(
        "../../../ados-config/tests/fixtures/sim-capture-config.yaml"
    ));
    let cfg = AgentVideoConfig::load_from(&path);
    let legs = cfg.resolve_legs(&CameraConfig::default());
    let sources: Vec<&str> = legs.iter().map(|l| l.source.as_str()).collect();
    assert_eq!(sources, ["framebus://cam_fl", "framebus://cam_fr"]);
    assert!(legs.iter().all(|l| l.raw_input().is_some()));
    let primary = cfg.primary_camera_config(CameraConfig::load_from(&path));
    assert_eq!(primary.framebus_camera(), Some("cam_fl"));
}
