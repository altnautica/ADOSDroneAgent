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
    // The leg decimates, so the input is declared at the leg's rate.
    assert!(has_pair(&cmd, "-framerate", "1"), "{cmd:?}");
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

/// The argv of a frame-bus leg at `fps` over frames captured at `raw_fps`:
/// the declared input rate and the filter chain.
fn leg_rate_and_filters(fps: u32, raw_fps: u32) -> (String, String) {
    let p = EncoderParams {
        width: 320,
        height: 180,
        fps,
        bitrate_kbps: 50,
        framebus_input: Some(RawInput {
            format: FrameFormat::Gray8,
            width: 320,
            height: 180,
            fps: raw_fps,
        }),
        ..params(EncoderKind::Ffmpeg, 320, 180, fps, 50)
    };
    let cmd = build_encoder_command(&p, SRC, RTSP_OUT, Some(&framebus_cam()), &rockchip()).unwrap();
    let after = |flag: &str| cmd[cmd.iter().position(|t| t == flag).expect(flag) + 1].clone();
    (after("-framerate"), after("-vf"))
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
    // The encoder is told the rate it really gets, so its rate control
    // spends the bitrate over one frame a second, not over twenty.
    assert!(has_pair(&cmd, "-framerate", "1"), "{cmd:?}");
    assert!(!has_pair(&cmd, "-framerate", "20"), "{cmd:?}");
    let vf = &cmd[cmd.iter().position(|t| t == "-vf").expect("-vf") + 1];
    assert!(
        vf.starts_with(crate::encoder::ffmpeg::FRAMEBUS_WALLCLOCK_PTS),
        "{vf}"
    );
    assert!(vf.contains(",select="), "{vf}");
    // A leg at the captured rate is declared at it and not decimated.
    let (rate, vf) = leg_rate_and_filters(20, 20);
    assert_eq!(rate, "20");
    assert!(!vf.contains("select="), "{vf}");
}

/// The real `ffmpeg` runs stamp frames with the wall clock, so they run one
/// at a time: a run beside another reads its pipe late under the load.
static REAL_TIME_FFMPEG: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Runs a frame-bus leg's input rate and filters through a real `ffmpeg`
/// with libx264 at 50 kb/s, writing 320x180 frames at `raw_fps` in real time
/// for each `(frames, pause_after_ms)` run. Returns each packet's
/// `(pts in 90 kHz ticks, bytes)`, or `None` when ffmpeg is not installed.
fn run_leg(fps: u32, raw_fps: u32, runs: &[(u32, u64)]) -> Option<Vec<(i64, i64)>> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let _one_at_a_time = REAL_TIME_FFMPEG.lock().unwrap_or_else(|e| e.into_inner());
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("ffmpeg not installed: leg run skipped");
        return None;
    }
    let (rate, vf) = leg_rate_and_filters(fps, raw_fps);
    let mut child = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-f", "rawvideo"])
        .args(["-pix_fmt", "gray", "-video_size", "320x180"])
        .args(["-framerate", &rate, "-i", "-", "-vf", &vf])
        .args(["-fps_mode", "passthrough", "-enc_time_base:v", "1/90000"])
        .args(["-c:v", "libx264", "-b:v", "50k", "-pix_fmt", "yuv420p"])
        .args(["-preset", "ultrafast", "-tune", "zerolatency", "-g", "1"])
        .args(["-f", "framecrc", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let period = std::time::Duration::from_micros(1_000_000 / u64::from(raw_fps));
    // A moving textured picture, so the encoder has detail to spend bits on.
    let mut n = 0u32;
    let mut due = std::time::Instant::now();
    for (frames, pause_ms) in runs {
        for _ in 0..*frames {
            let frame: Vec<u8> = (0..320 * 180u32)
                .map(|i| (((i % 320) + n * 4) ^ ((i / 320) * 2)) as u8)
                .collect();
            std::thread::sleep(due.saturating_duration_since(std::time::Instant::now()));
            stdin.write_all(&frame).unwrap();
            stdin.flush().unwrap();
            n += 1;
            due += period;
        }
        due += std::time::Duration::from_millis(*pause_ms);
    }
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| {
                let mut f = l.split(',').map(str::trim);
                let pts = f.nth(2)?.parse().ok()?;
                let size = f.nth(1)?.parse().ok()?;
                Some((pts, size))
            })
            .collect(),
    )
}

/// The 1 fps thumbnail over 20 fps frames through a real `ffmpeg`: frames
/// come out about once a second with a 4 s pause kept, never filled with
/// duplicates, and the stream holds its 50 kb/s budget per frame it sends.
#[test]
fn the_thumbnail_keeps_a_gap_a_gap_and_its_bitrate() {
    let Some(packets) = run_leg(1, 20, &[(120, 4000), (80, 0)]) else {
        return;
    };
    // 6 s of frames, the pause, then 4 s: about ten kept; a constant 1 fps
    // output would add four duplicates for the pause.
    assert!((8..=12).contains(&packets.len()), "{packets:?}");
    let steps_ms: Vec<i64> = packets.windows(2).map(|w| (w[1].0 - w[0].0) / 90).collect();
    assert!(steps_ms.iter().all(|s| *s >= 500), "{steps_ms:?}");
    assert!(steps_ms.iter().any(|s| *s >= 4000), "{steps_ms:?}");
    // One frame a second at 50 kb/s is about 6250 bytes a frame, once the
    // rate control has settled past its first few frames; an encoder told
    // the captured rate spends about a twentieth of that.
    let settled = &packets[4..];
    let bytes: i64 = settled.iter().map(|p| p.1).sum();
    let kbps = bytes as f64 * 8.0 / settled.len() as f64 / 1000.0;
    assert!((35.0..=65.0).contains(&kbps), "{kbps} kb/s {packets:?}");
}

/// A target above two thirds of the captured rate still decimates: 25 of
/// 30 fps keeps about 25 frames a second, not all 30.
#[test]
fn a_target_close_to_the_captured_rate_still_decimates() {
    let Some(packets) = run_leg(25, 30, &[(90, 0)]) else {
        return;
    };
    // 3 s of frames: 75 slots, against 90 frames written.
    assert!((70..=80).contains(&packets.len()), "{}", packets.len());
}

/// The stamp filter itself, run through a real `ffmpeg` when one is
/// installed: frames written after a long pause and then 120 ms apart come
/// out stamped with those pauses, not with frame count over a 30 fps clock.
#[test]
fn the_wallclock_stamp_follows_when_frames_arrive() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let _one_at_a_time = REAL_TIME_FFMPEG.lock().unwrap_or_else(|e| e.into_inner());
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
