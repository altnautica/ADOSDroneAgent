//! Encoder input selection for a `framebus://<camera_id>` source.

use super::*;
use ados_protocol::framebus::FrameFormat;

const SRC: &str = "framebus://cam_fl";

fn fifo() -> String {
    crate::framebus_input::fifo_path("cam_fl")
        .to_string_lossy()
        .into_owned()
}

fn framebus_params(kind: EncoderKind, format: Option<FrameFormat>) -> EncoderParams {
    EncoderParams {
        framebus_input: format.map(|format| RawInput {
            format,
            width: 1280,
            height: 800,
            fps: 20,
        }),
        ..params(kind, 1280, 800, 20, 4000)
    }
}

fn framebus_cam() -> CameraInfo {
    CameraInfo {
        camera_type: CameraType::Framebus,
        device_path: SRC.into(),
        capabilities: vec!["framebus".into()],
    }
}

#[test]
fn a_framebus_source_is_read_as_raw_video_from_its_pipe() {
    let p = framebus_params(EncoderKind::Ffmpeg, Some(FrameFormat::Gray8));
    let cmd = build_encoder_command(&p, SRC, RTSP_OUT, Some(&framebus_cam()), &rockchip()).unwrap();
    assert_eq!(cmd[0], "ffmpeg");
    assert!(has_pair(&cmd, "-f", "rawvideo"), "{cmd:?}");
    assert!(has_pair(&cmd, "-pix_fmt", "gray"), "{cmd:?}");
    assert!(has_pair(&cmd, "-video_size", "1280x800"), "{cmd:?}");
    assert!(has_pair(&cmd, "-framerate", "20"), "{cmd:?}");
    assert!(has_pair(&cmd, "-i", &fifo()), "{cmd:?}");
    // The device is the vision engine's: never a V4L2 input, never the URI.
    assert!(!has_pair(&cmd, "-f", "v4l2"), "{cmd:?}");
    assert!(!cmd.iter().any(|t| t.contains("framebus://")), "{cmd:?}");
    assert!(has_pair(&cmd, "-c:v", "libx264"), "{cmd:?}");
    assert!(
        !cmd.iter().any(|t| t == "-s"),
        "no rescale at the captured size: {cmd:?}"
    );
}

#[test]
fn an_attention_switch_rescales_instead_of_misreading_the_pipe() {
    let p = EncoderParams {
        width: 320,
        height: 180,
        fps: 1,
        ..framebus_params(EncoderKind::Ffmpeg, Some(FrameFormat::Gray8))
    };
    let cmd = build_encoder_command(&p, SRC, RTSP_OUT, Some(&framebus_cam()), &rockchip()).unwrap();
    assert!(has_pair(&cmd, "-video_size", "1280x800"), "{cmd:?}");
    assert!(has_pair(&cmd, "-framerate", "20"), "{cmd:?}");
    assert!(has_pair(&cmd, "-s", "320x180"), "{cmd:?}");
    assert!(!cmd.iter().any(|t| t == "-r"), "{cmd:?}");
    let line = build_encoder_command(&p, SRC, RTSP_OUT, Some(&framebus_cam()), &rk_mpp())
        .unwrap()
        .join(" ");
    assert!(
        line.contains("rawvideoparse width=1280 height=800"),
        "{line}"
    );
    assert!(
        line.contains("video/x-raw,format=NV12,width=320,height=180"),
        "{line}"
    );
}

#[test]
fn a_framebus_encoder_keeps_the_frames_real_timing() {
    let p = framebus_params(EncoderKind::Ffmpeg, Some(FrameFormat::Rgb24));
    let cmd = build_encoder_command(&p, SRC, RTSP_OUT, Some(&framebus_cam()), &rockchip()).unwrap();
    // Stamped with the time each frame left the pipe, passed through as is:
    // no synthetic constant rate re-gridding a burst or a gap into even steps.
    assert!(has_pair(&cmd, "-fps_mode", "passthrough"), "{cmd:?}");
    assert!(has_pair(&cmd, "-enc_time_base:v", "1/90000"), "{cmd:?}");
    assert!(
        !cmd.iter().any(|t| t == "-r"),
        "no output rate cap at the captured rate: {cmd:?}"
    );
    let vf = cmd[cmd.iter().position(|t| t == "-vf").expect("-vf") + 1].clone();
    assert!(
        vf.starts_with("settb=1/90000,setpts=") && vf.contains("RTCTIME"),
        "{vf}"
    );
    // Orientation still applies after the stamp, in the same filter chain.
    let rotated = EncoderParams {
        rotation: 180,
        ..p.clone()
    };
    let cmd =
        build_encoder_command(&rotated, SRC, RTSP_OUT, Some(&framebus_cam()), &rockchip()).unwrap();
    let vf = &cmd[cmd.iter().position(|t| t == "-vf").unwrap() + 1];
    assert!(vf.ends_with(",transpose=2,transpose=2"), "{vf}");
    assert_eq!(cmd.iter().filter(|t| *t == "-vf").count(), 1, "{cmd:?}");
}

#[test]
fn a_device_camera_keeps_its_output_rate_cap() {
    let p = params(EncoderKind::Ffmpeg, 1280, 720, 30, 4000);
    let cam = CameraInfo {
        camera_type: CameraType::Usb,
        device_path: "/dev/video0".into(),
        capabilities: vec![],
    };
    let cmd = build_encoder_command(&p, "/dev/video0", RTSP_OUT, Some(&cam), &rockchip()).unwrap();
    assert!(has_pair(&cmd, "-r", "30"), "{cmd:?}");
    assert!(!cmd.iter().any(|t| t == "-fps_mode"), "{cmd:?}");
    assert!(!cmd.iter().any(|t| t.contains("RTCTIME")), "{cmd:?}");
}

#[test]
fn a_framebus_thumbnail_decimates_without_filling_gaps() {
    let p = EncoderParams {
        width: 320,
        height: 180,
        fps: 1,
        ..framebus_params(EncoderKind::Ffmpeg, Some(FrameFormat::Gray8))
    };
    let cmd = build_encoder_command(&p, SRC, RTSP_OUT, Some(&framebus_cam()), &rockchip()).unwrap();
    // A constant output rate would duplicate the last frame across a gap.
    assert!(!cmd.iter().any(|t| t == "-r"), "{cmd:?}");
    assert!(has_pair(&cmd, "-fps_mode", "passthrough"), "{cmd:?}");
    assert!(has_pair(&cmd, "-enc_time_base:v", "1/90000"), "{cmd:?}");
    let vf = &cmd[cmd.iter().position(|t| t == "-vf").expect("-vf") + 1];
    assert!(
        vf.starts_with(crate::encoder::ffmpeg::FRAMEBUS_WALLCLOCK_PTS),
        "{vf}"
    );
    assert!(
        vf.ends_with(",select=isnan(prev_selected_t)+gte(t-prev_selected_t\\,39/40)"),
        "{vf}"
    );
}

/// The thumbnail decimation run through a real `ffmpeg` when one is
/// installed: 20 fps frames, a 6 s pause, then frames again come out about
/// once a second with the pause kept, never filled with duplicates.
#[test]
fn the_thumbnail_decimation_keeps_a_gap_a_gap() {
    use std::io::Write;
    use std::process::{ChildStdin, Command, Stdio};
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("ffmpeg not installed: decimation run skipped");
        return;
    }
    let vf = format!(
        "{},{}",
        crate::encoder::ffmpeg::FRAMEBUS_WALLCLOCK_PTS,
        crate::encoder::ffmpeg::framebus_decimate(1, 20)
    );
    let mut child = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-f", "rawvideo"])
        .args(["-pix_fmt", "gray", "-video_size", "4x2", "-framerate", "20"])
        .args(["-i", "-", "-vf", &vf])
        .args(["-fps_mode", "passthrough", "-enc_time_base:v", "1/90000"])
        .args(["-f", "framecrc", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let burst = |stdin: &mut ChildStdin| {
        for _ in 0..40 {
            stdin.write_all(&[7u8; 8]).unwrap();
            stdin.flush().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    };
    burst(&mut stdin);
    std::thread::sleep(std::time::Duration::from_secs(6));
    burst(&mut stdin);
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    let pts: Vec<i64> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split(',').nth(2)?.trim().parse().ok())
        .collect();
    // 2 s of frames on each side of the pause: about two kept per side; a
    // constant 1 fps output would add six duplicates for the pause.
    assert!((3..=6).contains(&pts.len()), "{pts:?}");
    let steps_ms: Vec<i64> = pts.windows(2).map(|w| (w[1] - w[0]) / 90).collect();
    assert!(steps_ms.iter().all(|s| *s >= 900), "{steps_ms:?}");
    assert!(steps_ms.iter().any(|s| *s >= 6000), "{steps_ms:?}");
}

/// The stamp filter itself, run through a real `ffmpeg` when one is
/// installed: frames written after a long pause and then 120 ms apart come
/// out stamped with those pauses, not with frame count over a 30 fps clock.
#[test]
fn the_wallclock_stamp_follows_when_frames_arrive() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("ffmpeg not installed: stamp filter run skipped");
        return;
    }
    let mut child = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-f", "rawvideo"])
        .args(["-pix_fmt", "gray", "-video_size", "4x2", "-framerate", "30"])
        .args([
            "-i",
            "-",
            "-vf",
            crate::encoder::ffmpeg::FRAMEBUS_WALLCLOCK_PTS,
        ])
        .args(["-fps_mode", "passthrough", "-enc_time_base:v", "1/90000"])
        .args(["-f", "framecrc", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for (i, pause_ms) in [0u64, 300, 120, 120].iter().enumerate() {
        std::thread::sleep(std::time::Duration::from_millis(*pause_ms));
        stdin.write_all(&[i as u8; 8]).unwrap();
        stdin.flush().unwrap();
    }
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    let pts: Vec<i64> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split(',').nth(2)?.trim().parse().ok())
        .collect();
    assert!(pts.len() >= 3, "{pts:?}");
    let steps_ms: Vec<i64> = pts.windows(2).map(|w| (w[1] - w[0]) / 90).collect();
    // The demuxer may hold back the first frame; the last two steps are the
    // real 120 ms pauses, never the 33 ms a constant 30 fps clock would give.
    for step in &steps_ms[steps_ms.len() - 2..] {
        assert!((90..=250).contains(step), "{steps_ms:?}");
    }
}

#[test]
fn a_framebus_source_takes_the_rockchip_vpu_when_mpp_is_present() {
    let p = framebus_params(EncoderKind::Ffmpeg, Some(FrameFormat::Rgb24));
    let cmd = build_encoder_command(&p, SRC, RTSP_OUT, Some(&framebus_cam()), &rk_mpp()).unwrap();
    assert_eq!(cmd[0], "gst-launch-1.0", "{cmd:?}");
    let line = cmd.join(" ");
    // Stamped on arrival from the feed, not by frame count.
    assert!(
        line.contains(&format!("filesrc location={} do-timestamp=true", fifo())),
        "{line}"
    );
    assert!(
        line.contains("rawvideoparse width=1280 height=800 format=rgb framerate=20/1"),
        "{line}"
    );
    assert!(
        line.contains("video/x-raw,format=NV12,width=1280,height=800"),
        "{line}"
    );
    assert!(line.contains("mpph264enc"), "{line}");
    assert!(line.contains("rc-mode=vbr"), "{line}");
    assert!(!line.contains("v4l2src"), "{line}");
    assert!(!line.contains("rga"), "{line}");
    assert_eq!(
        encoder_label(EncoderKind::Gstreamer, &cmd),
        "gstreamer-mpph264enc"
    );
}

#[test]
fn the_forced_software_fallback_leaves_the_vpu() {
    // What the no-first-packet ladder does after the VPU produced nothing.
    let p = EncoderParams {
        encoder: "software".into(),
        ..framebus_params(EncoderKind::Ffmpeg, Some(FrameFormat::Gray8))
    };
    let cmd = build_encoder_command(&p, SRC, RTSP_OUT, Some(&framebus_cam()), &rk_mpp()).unwrap();
    assert_eq!(cmd[0], "ffmpeg", "{cmd:?}");
    assert!(has_pair(&cmd, "-c:v", "libx264"), "{cmd:?}");
    assert!(has_pair(&cmd, "-i", &fifo()), "{cmd:?}");
    // On a GStreamer-only board the fallback stays in the family, in software.
    let gst = EncoderParams {
        kind: EncoderKind::Gstreamer,
        ..p
    };
    let line = build_encoder_command(&gst, SRC, RTSP_OUT, Some(&framebus_cam()), &rk_mpp())
        .unwrap()
        .join(" ");
    assert!(
        line.contains("x264enc") && !line.contains("mpph264enc"),
        "{line}"
    );
}

#[test]
fn a_framebus_source_never_takes_the_v4l2src_omx_path() {
    let p = framebus_params(EncoderKind::Ffmpeg, Some(FrameFormat::Nv12));
    let cmd =
        build_encoder_command(&p, SRC, RTSP_OUT, Some(&framebus_cam()), &allwinner_omx()).unwrap();
    assert_eq!(cmd[0], "ffmpeg", "{cmd:?}");
    assert!(has_pair(&cmd, "-pix_fmt", "nv12"), "{cmd:?}");
}

#[test]
fn a_framebus_source_without_a_format_is_refused() {
    let p = framebus_params(EncoderKind::Ffmpeg, None);
    assert_eq!(
        build_encoder_command(&p, SRC, RTSP_OUT, Some(&framebus_cam()), &rockchip()),
        Err(EncoderError::MissingFramebusFormat(SRC.into()))
    );
}

#[test]
fn framebus_detection_and_input_locator() {
    assert_eq!(
        detect_encoder_for_camera(CameraType::Framebus, false, true, true),
        Some(EncoderKind::Ffmpeg)
    );
    assert_eq!(
        detect_encoder_for_camera(CameraType::Framebus, false, false, true),
        Some(EncoderKind::Gstreamer)
    );
    assert_eq!(input_locator(SRC), fifo());
    assert_eq!(input_locator("/dev/video0"), "/dev/video0");
}
