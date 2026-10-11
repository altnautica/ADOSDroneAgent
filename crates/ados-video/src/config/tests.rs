use super::*;

#[test]
fn defaults_match_python() {
    let c = CameraConfig::default();
    assert_eq!(c.source, "csi");
    assert_eq!(c.codec, "h264");
    assert_eq!(c.width, 1280);
    assert_eq!(c.height, 720);
    assert_eq!(c.fps, 30);
    assert_eq!(c.bitrate_kbps, 4000);
    // Encode-plane keys: image transform off, encoder auto (probe), and the
    // keyframe interval 0 meaning the encoder picks a short low-latency GOP.
    assert_eq!(c.rotation, 0);
    assert!(!c.hflip);
    assert!(!c.vflip);
    assert_eq!(c.encoder, "auto");
    assert_eq!(c.keyframe_interval, 0);
}

#[test]
fn partial_yaml_fills_defaults() {
    let yaml = "video:\n  camera:\n    width: 1920\n    height: 1080\n    fps: 60\n";
    let raw: serde_norway::Value = serde_norway::from_str(yaml).unwrap();
    // Round-trip through the same nested shape the loader uses.
    let cfg: CameraConfig = {
        #[derive(serde::Deserialize, Default)]
        struct R {
            #[serde(default)]
            video: V,
        }
        #[derive(serde::Deserialize, Default)]
        struct V {
            #[serde(default)]
            camera: CameraConfig,
        }
        let r: R = serde_norway::from_value(raw).unwrap();
        r.video.camera
    };
    assert_eq!(cfg.width, 1920);
    assert_eq!(cfg.height, 1080);
    assert_eq!(cfg.fps, 60);
    // Untouched fields fall back to defaults.
    assert_eq!(cfg.codec, "h264");
    assert_eq!(cfg.bitrate_kbps, 4000);
}

#[test]
fn network_source_detects_rtsp_and_http() {
    let mut c = CameraConfig::default();
    // The default hint is a local camera → discovery path.
    assert_eq!(c.network_source(), None);
    for hint in ["csi", "usb", "ip", "/dev/video0"] {
        c.source = hint.to_string();
        assert_eq!(c.network_source(), None, "{hint} is not a network source");
    }
    c.source = "rtsp://10.0.0.9:554/live".to_string();
    assert_eq!(c.network_source(), Some("rtsp://10.0.0.9:554/live"));
    c.source = "http://cam.local:8080/stream".to_string();
    assert_eq!(c.network_source(), Some("http://cam.local:8080/stream"));
    // The TLS forms of RTSP and HTTP are network sources too.
    c.source = "rtsps://10.0.0.9:322/live".to_string();
    assert_eq!(c.network_source(), Some("rtsps://10.0.0.9:322/live"));
    c.source = "https://cam.local/stream".to_string();
    assert_eq!(c.network_source(), Some("https://cam.local/stream"));
    // Surrounding whitespace is trimmed.
    c.source = "  rtsp://host/main  ".to_string();
    assert_eq!(c.network_source(), Some("rtsp://host/main"));
}

// --- AgentVideoConfig ---------------------------------------------

fn write_tmp(yaml: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    std::fs::write(&path, yaml).unwrap();
    (dir, path)
}

#[test]
fn agent_video_defaults_on_missing_file() {
    let c = AgentVideoConfig::load_from(std::path::Path::new("/nonexistent/config.yaml"));
    assert_eq!(c.mode, "wfb");
    assert!(c.profile.is_none());
    assert!(c.cloud_relay_url.is_none());
    assert!(!c.wfb.sei_latency);
    assert!(!c.is_ground_station());
    assert!(!c.is_disabled());
    assert!(!c.cloud_enabled());
    // The vision tap is off by default with the documented defaults.
    assert!(!c.vision.enabled);
    assert_eq!(c.vision.fps, 10);
    assert_eq!(c.vision.width, 640);
    assert_eq!(c.vision.height, 480);
    assert_eq!(c.vision.format, "rgb24");
    assert_eq!(c.vision.sink, "/run/ados/vision-tap-main.sock");
}

#[test]
fn vision_tap_loads_full_block() {
    let yaml = "\
video:
  vision:
    enabled: true
    fps: 5
    width: 1280
    height: 720
    format: nv12
    sink: /run/ados/custom.sock
";
    let (_dir, path) = write_tmp(yaml);
    let c = AgentVideoConfig::load_from(&path);
    assert!(c.vision.enabled);
    assert_eq!(c.vision.fps, 5);
    assert_eq!(c.vision.width, 1280);
    assert_eq!(c.vision.height, 720);
    assert_eq!(c.vision.format, "nv12");
    assert_eq!(c.vision.sink, "/run/ados/custom.sock");
    assert_eq!(c.vision.pixel_format(), "nv12");
}

#[test]
fn vision_tap_partial_block_fills_defaults() {
    // Only enabled flipped on; everything else must take the defaults.
    let yaml = "video:\n  vision:\n    enabled: true\n";
    let (_dir, path) = write_tmp(yaml);
    let c = AgentVideoConfig::load_from(&path);
    assert!(c.vision.enabled);
    assert_eq!(c.vision.fps, 10);
    assert_eq!(c.vision.width, 640);
    assert_eq!(c.vision.height, 480);
    assert_eq!(c.vision.format, "rgb24");
    assert_eq!(c.vision.sink, "/run/ados/vision-tap-main.sock");
}

#[test]
fn vision_pixel_format_falls_back_on_garbage() {
    let mut v = VisionTapConfig::default();
    assert_eq!(v.pixel_format(), "rgb24");
    v.format = "yuv420p".to_string();
    assert_eq!(v.pixel_format(), "yuv420p");
    v.format = "bogus".to_string();
    assert_eq!(v.pixel_format(), "rgb24");
}

#[test]
fn agent_video_loads_extra_fields() {
    let yaml = "\
agent:
  profile: drone
video:
  mode: cloud
  cloud_relay_url: rtsp://relay.example.com:8554
  wfb:
    sei_latency: true
";
    let (_dir, path) = write_tmp(yaml);
    let c = AgentVideoConfig::load_from(&path);
    assert_eq!(c.mode, "cloud");
    assert_eq!(c.profile.as_deref(), Some("drone"));
    assert_eq!(
        c.cloud_relay_url.as_deref(),
        Some("rtsp://relay.example.com:8554")
    );
    assert!(c.wfb.sei_latency);
    assert!(c.cloud_enabled());
    assert!(!c.is_disabled());
}

#[test]
fn agent_video_partial_config_fills_defaults() {
    // Only a width override under camera + a bare video.mode; everything
    // else must default (never block on a partial config).
    let yaml = "video:\n  mode: disabled\n";
    let (_dir, path) = write_tmp(yaml);
    let c = AgentVideoConfig::load_from(&path);
    assert_eq!(c.mode, "disabled");
    assert!(c.is_disabled());
    assert!(c.profile.is_none());
}

#[test]
fn agent_video_ground_station_gate() {
    let yaml = "agent:\n  profile: ground_station\n";
    let (_dir, path) = write_tmp(yaml);
    let c = AgentVideoConfig::load_from(&path);
    assert!(c.is_ground_station());
    // Hyphen wire form is accepted too.
    let yaml2 = "agent:\n  profile: ground-station\n";
    let (_dir2, path2) = write_tmp(yaml2);
    assert!(AgentVideoConfig::load_from(&path2).is_ground_station());
}

#[test]
fn agent_video_profile_override_precedence() {
    // video.profile overrides agent.profile when both are set.
    let yaml = "\
agent:
  profile: drone
video:
  profile: ground_station
";
    let (_dir, path) = write_tmp(yaml);
    let c = AgentVideoConfig::load_from(&path);
    assert!(c.is_ground_station());
}

#[test]
fn agent_video_empty_cloud_url_is_not_enabled() {
    let yaml = "video:\n  cloud_relay_url: \"\"\n";
    let (_dir, path) = write_tmp(yaml);
    let c = AgentVideoConfig::load_from(&path);
    assert!(!c.cloud_enabled());
}

#[test]
fn agent_video_malformed_yaml_falls_back_to_default() {
    let yaml = "this: : is not [valid yaml at all }}}";
    let (_dir, path) = write_tmp(yaml);
    let c = AgentVideoConfig::load_from(&path);
    // Unparseable → defaults, never a panic / block.
    assert_eq!(c.mode, "wfb");
    assert!(c.profile.is_none());
}

// --- video.cameras[] / resolve_legs -------------------------------

#[test]
fn cameras_absent_synthesizes_single_main_leg() {
    // No `video.cameras` ⇒ one primary "main" leg from the legacy block,
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
video:
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
video:
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
video:
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
video:
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
video:
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
video:
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
    let yaml = "video:\n  cameras:\n    - { source: /dev/video0 }\n";
    let (_dir, path) = write_tmp(yaml);
    let cfg = AgentVideoConfig::load_from(&path);
    assert_eq!(cfg.cameras.len(), 1);
    assert_eq!(cfg.cameras[0].id, "main");
}

#[test]
fn camera_match_csi_fingerprint_parses() {
    let yaml = "\
video:
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

// --- RosterVideoConfig (quiet single-pass roster loader) ----------

#[test]
fn roster_config_loads_profile_camera_presence_and_legs_in_one_pass() {
    // The camera block present ⇒ Some (a real single-camera drone); the
    // cameras[] list and the resolved profile parse in the same pass. This is
    // the quiet loader the roster route uses (no config-status side-effect —
    // it calls the quiet yaml_or_default, never write_config_status).
    let yaml = "\
agent:
  profile: drone
video:
  camera: { source: usb, codec: h265 }
  cameras:
    - { id: eo, source: /dev/video0, role: primary }
    - { id: ir, source: rtsp://pod/ir, role: ir }
";
    let (_dir, path) = write_tmp(yaml);
    let cfg = RosterVideoConfig::load_from(&path);
    assert_eq!(cfg.profile.as_deref(), Some("drone"));
    assert!(!cfg.is_ground_station());
    let camera = cfg.camera.expect("a declared video.camera block is Some");
    assert_eq!(camera.source, "usb");
    assert_eq!(camera.codec, "h265");
    assert_eq!(cfg.cameras.len(), 2);
    assert_eq!(cfg.cameras[0].id, "eo");
}

#[test]
fn roster_config_camera_absent_is_none() {
    // No `video.camera` block ⇒ None, so the roster can distinguish a
    // camera-less node from a real single-camera drone.
    let yaml = "agent:\n  profile: ground_station\nvideo:\n  mode: wfb\n";
    let (_dir, path) = write_tmp(yaml);
    let cfg = RosterVideoConfig::load_from(&path);
    assert!(cfg.camera.is_none());
    assert!(cfg.cameras.is_empty());
    assert!(cfg.is_ground_station());
}

#[test]
fn roster_config_profile_override_and_defaults() {
    // video.profile overrides agent.profile; a missing/malformed file yields
    // the empty default (no profile, no camera, no legs) and never blocks.
    let yaml = "agent:\n  profile: drone\nvideo:\n  profile: ground-station\n";
    let (_dir, path) = write_tmp(yaml);
    assert!(RosterVideoConfig::load_from(&path).is_ground_station());
    let missing = RosterVideoConfig::load_from(std::path::Path::new("/nope/config.yaml"));
    assert!(missing.profile.is_none());
    assert!(missing.camera.is_none());
    assert!(missing.cameras.is_empty());
    let (_bad_dir, bad) = write_tmp("this: : not [valid yaml }}}");
    let malformed = RosterVideoConfig::load_from(&bad);
    assert!(malformed.camera.is_none());
    assert!(malformed.cameras.is_empty());
}

#[test]
fn resolved_leg_ownership_and_camera_config() {
    let yaml = "\
video:
  cameras:
    - { id: main, source: /dev/video0, role: eo, codec: h264 }
    - { id: belly, source: /dev/video1, role: eo_wide, codec: h265, width: 640, height: 480, fps: 15 }
    - { id: ir, source: rtsp://pod/ir, role: ir }
";
    let (_dir, path) = write_tmp(yaml);
    let legs = AgentVideoConfig::load_from(&path).resolve_legs(&CameraConfig::default());
    // Primary + local secondary own an encoder; the network secondary is a pull.
    assert!(legs[0].is_owned_encoder()); // main (primary)
    assert!(legs[1].is_owned_encoder()); // belly (local secondary)
    assert!(!legs[2].is_owned_encoder()); // ir (network pull)
                                          // The local secondary's CameraConfig view carries its own geometry.
    let cam = legs[1].to_camera_config();
    assert_eq!(cam.source, "/dev/video1");
    assert_eq!(cam.codec, "h265");
    assert_eq!(cam.width, 640);
    assert_eq!(cam.fps, 15);
}

// --- encode-plane keys propagate to the encoder --------------------

#[test]
fn camera_leg_encode_plane_keys_reach_to_camera_config() {
    // The image-transform + encoder-selection keys are the ONE set of
    // per-leg keys the pipeline reads, so unlike the roster metadata they
    // must survive `resolve_legs` → `to_camera_config` (the per-leg encoder
    // command is built from that CameraConfig view).
    let yaml = "\
video:
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

/// The `/video/camera/thumbnail` slice default agrees with the schema.
#[test]
fn config_slice_conforms() {
    ados_config::schema::assert_slice_conforms::<CameraConfig>("/video/camera");
}
