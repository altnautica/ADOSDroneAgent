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

pub(super) fn write_tmp(yaml: &str) -> (tempfile::TempDir, std::path::PathBuf) {
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

/// The `/video/camera/thumbnail` slice default agrees with the schema.
#[test]
fn config_slice_conforms() {
    ados_config::schema::assert_slice_conforms::<CameraConfig>("/video/camera");
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
