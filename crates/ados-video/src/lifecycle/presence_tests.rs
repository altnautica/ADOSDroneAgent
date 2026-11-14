//! What the camera-state sidecar says about captured and secondary cameras,
//! and the camera a secondary leg's encoder is given.

use super::*;
use crate::camera_presence::PresenceSources;
use crate::camera_state::PipelineOutcome;
use crate::config::{AgentVideoConfig, CameraConfig, CameraLeg};
use crate::encoder::EncoderEnv;
use ados_protocol::hwcaps::Probed;

fn env() -> EncoderEnv {
    EncoderEnv {
        hw_h264: Probed::NotProbed,
        has_mpph264enc: false,
        has_omxh264videoenc: false,
        has_rtspclientsink: false,
        encoder_api: "unknown".into(),
        pi5_class: false,
        cpu_threads: 4,
    }
}

fn orchestrator(cameras_yaml: &str) -> VideoOrchestrator {
    let cameras: Vec<CameraLeg> = serde_norway::from_str(cameras_yaml).unwrap();
    let config = AgentVideoConfig {
        cameras,
        ..AgentVideoConfig::default()
    };
    VideoOrchestrator::new(
        config,
        CameraConfig::default(),
        std::path::Path::new("/tmp"),
        env(),
    )
}

fn sidecar(path: &std::path::Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[tokio::test]
async fn a_captured_camera_whose_device_vanishes_reads_missing() {
    let dir = tempfile::tempdir().unwrap();
    let node = dir.path().join("video0");
    std::fs::write(&node, b"").unwrap();
    let belly = dir.path().join("video2");
    std::fs::write(&belly, b"").unwrap();
    let mut o = orchestrator(&format!(
        "- {{id: front, role: primary, source: {}, capture: true}}\n\
         - {{id: belly, role: down, source: {}, capture: true}}\n",
        node.display(),
        belly.display()
    ));
    let state_path = dir.path().join("camera-state.json");
    o.camera_state_path = Some(state_path.clone());
    o.presence_sources = PresenceSources {
        sys_v4l: dir.path().join("sys_v4l"),
        vision_status: dir.path().join("vision-status.json"),
    };
    o.refresh_discovery(true).await;

    o.persist_pipeline_outcome(PipelineOutcome::Streaming).await;
    let v = sidecar(&state_path);
    assert_eq!(v["state"], "ready");
    assert_eq!(v["cameras"][0]["id"], "front");
    assert_eq!(v["cameras"][0]["state"], "ready");
    assert_eq!(
        v["cameras"][0]["device_path"],
        node.to_string_lossy().as_ref()
    );
    assert_eq!(v["cameras"][1]["id"], "belly");
    assert_eq!(v["cameras"][1]["primary"], false);

    // The belly camera drops off the bus: its own entry says so, the primary
    // stays ready.
    std::fs::remove_file(&belly).unwrap();
    o.persist_pipeline_outcome(PipelineOutcome::Streaming).await;
    let v = sidecar(&state_path);
    assert_eq!(v["state"], "ready");
    assert_eq!(v["cameras"][1]["state"], "missing");

    // The captured primary drops off: the sidecar's camera state is missing,
    // not the `ready` the frame-bus stand-in used to carry.
    std::fs::remove_file(&node).unwrap();
    o.persist_pipeline_outcome(PipelineOutcome::Streaming).await;
    let v = sidecar(&state_path);
    assert_eq!(v["state"], "missing");
    assert_eq!(v["cameras"][0]["state"], "missing");
    assert_eq!(v["cameras"][0]["captured"], true);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    assert_eq!(
        crate::camera_state::effective_state(&v, now, 30.0),
        Some(crate::camera_state::CameraState::Missing)
    );
}

#[test]
fn a_secondary_leg_is_given_the_capabilities_discovery_found() {
    let mut o = orchestrator(
        "- {id: main, role: primary, source: /dev/video0}\n\
         - {id: belly, role: down, source: /dev/video2}\n",
    );
    o.last_cameras = serde_json::from_str(
        r#"{"cameras":[
            {"name":"front","type":"usb","device_path":"/dev/video0","capabilities":["mjpeg","yuyv"]},
            {"name":"belly","type":"usb","device_path":"/dev/video2","capabilities":["yuyv"],
             "formats":[{"fourcc":"yuyv","width":640,"height":480,"fps":[30.0]}]}],
          "primary":{"device_path":"/dev/video0","name":"front"},"total_cameras":2}"#,
    )
    .unwrap();
    let cam = o.secondary_camera_info("/dev/video2", CameraType::Usb);
    assert_eq!(cam.capabilities, vec!["yuyv"]);
    assert_eq!(cam.device_path, "/dev/video2");

    // What the belly encoder then asks the camera for.
    let leg = o.legs.iter().find(|l| l.id == "belly").unwrap().clone();
    let params = EncoderParams::from_camera_config(
        crate::encoder::EncoderKind::Ffmpeg,
        &leg.to_camera_config(),
    );
    let cmd = build_encoder_command(
        &params,
        &leg.source,
        "rtsp://localhost:8554/belly",
        Some(&cam),
        &o.env,
    )
    .unwrap();
    let at = cmd.iter().position(|t| t == "-input_format").expect("set");
    assert_eq!(cmd[at + 1], "yuyv422");

    // A device discovery did not list gets no list, and no forced format.
    let unknown = o.secondary_camera_info("/dev/video7", CameraType::Usb);
    assert!(unknown.capabilities.is_empty());
}
