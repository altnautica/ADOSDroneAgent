//! The discovery `formats` list and the capture format the direct encode asks
//! the camera for.

use super::*;
use crate::encoder::{build_encoder_command, EncoderEnv, EncoderKind, EncoderParams};
use ados_protocol::hwcaps::Probed;

fn usb_discovery(caps: &str, formats: &str) -> DiscoveryResult {
    let json = format!(
        r#"{{"cameras":[{{"name":"UVC","type":"usb","device_path":"/dev/video2","width":1280,"height":720,"capabilities":{caps},"formats":{formats},"hardware_role":"camera"}}],"primary":{{"device_path":"/dev/video2","name":"UVC"}},"total_cameras":1}}"#
    );
    parse_discovery_stdout(&json).expect("parses")
}

fn software_env() -> EncoderEnv {
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

/// The `-input_format` the primary's direct encode passes, if any.
fn input_format(r: &DiscoveryResult) -> Option<String> {
    let cam = r.primary_camera_info().expect("primary");
    let mut p = EncoderParams::from_camera_config(EncoderKind::Ffmpeg, &Default::default());
    p.width = 1280;
    p.height = 720;
    p.fps = 30;
    let cmd = build_encoder_command(
        &p,
        "/dev/video2",
        "rtsp://127.0.0.1:8554/main",
        Some(&cam),
        &software_env(),
    )
    .unwrap();
    let at = cmd.iter().position(|t| t == "-input_format")?;
    Some(cmd[at + 1].clone())
}

const YUYV_ONLY: &str = r#"[{"fourcc":"yuyv","width":1280,"height":720,"fps":[10.0,5.0]},{"fourcc":"yuyv","width":640,"height":480,"fps":[30.0]}]"#;
const BOTH: &str = r#"[{"fourcc":"yuyv","width":640,"height":480,"fps":[30.0]},{"fourcc":"mjpeg","width":1280,"height":720,"fps":[30.0,15.0]}]"#;

#[test]
fn the_formats_list_parses_and_is_optional() {
    let r = usb_discovery(r#"["yuyv","mjpeg"]"#, BOTH);
    let cam = &r.cameras[0];
    assert_eq!(cam.formats.len(), 2);
    assert_eq!(cam.formats[1].fourcc, "mjpeg");
    assert_eq!((cam.formats[1].width, cam.formats[1].height), (1280, 720));
    assert_eq!(cam.formats[1].fps, vec![30.0, 15.0]);
    // A discovery that predates the list still parses, with an empty one.
    let old = parse_discovery_stdout(
        r#"{"cameras":[{"name":"x","type":"usb","device_path":"/dev/video2","capabilities":["mjpeg"]}],"primary":null,"total_cameras":1}"#,
    )
    .expect("parses");
    assert!(old.cameras[0].formats.is_empty());
    // An entry with unknown extra keys and no rates parses too.
    let sparse = usb_discovery(r#"["h264"]"#, r#"[{"fourcc":"h264","extra":1}]"#);
    assert!(sparse.cameras[0].formats[0].fps.is_empty());
}

#[test]
fn a_yuyv_only_camera_is_opened_as_yuyv422() {
    let r = usb_discovery(r#"["yuyv"]"#, YUYV_ONLY);
    assert_eq!(input_format(&r).as_deref(), Some("yuyv422"));
}

#[test]
fn an_mjpeg_camera_is_opened_as_mjpeg() {
    let r = usb_discovery(r#"["yuyv","mjpeg"]"#, BOTH);
    assert_eq!(input_format(&r).as_deref(), Some("mjpeg"));
}

#[test]
fn a_camera_offering_neither_lets_ffmpeg_choose() {
    let r = usb_discovery(
        r#"["h264"]"#,
        r#"[{"fourcc":"h264","width":1280,"height":720,"fps":[30.0]}]"#,
    );
    assert_eq!(input_format(&r), None);
}

#[test]
fn the_offered_formats_stand_in_for_a_missing_capability_list() {
    let r = usb_discovery("[]", YUYV_ONLY);
    let cam = r.primary_camera_info().unwrap();
    assert_eq!(cam.capabilities, vec!["yuyv"]);
    assert_eq!(input_format(&r).as_deref(), Some("yuyv422"));
}

#[test]
fn a_secondary_leg_finds_its_capabilities_by_device_path() {
    let r = usb_discovery(r#"["yuyv"]"#, YUYV_ONLY);
    let cam = r.camera_info_for("/dev/video2").expect("listed");
    assert_eq!(cam.capabilities, vec!["yuyv"]);
    assert!(r.camera_info_for("/dev/video7").is_none());
    // A stable link to the node finds the node discovery listed.
    let dir = tempfile::tempdir().unwrap();
    let node = dir.path().join("video2");
    std::fs::write(&node, b"").unwrap();
    let link = dir.path().join("usb-cam-index0");
    std::os::unix::fs::symlink(&node, &link).unwrap();
    let mut listed = r.clone();
    listed.cameras[0].device_path = node.to_string_lossy().into_owned();
    let via_link = listed
        .camera_info_for(&link.to_string_lossy())
        .expect("resolved");
    assert_eq!(via_link.capabilities, vec!["yuyv"]);
}
