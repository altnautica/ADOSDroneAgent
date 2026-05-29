//! Vision frame tap: an additive, optional leg that feeds raw decoded frames
//! to the on-box vision engine.
//!
//! The tap is a SEPARATE consumer of the same local mediamtx RTSP `/main`
//! stream the wfb radio fan-out reads. It never alters the encode output or the
//! `wfb_tee` RTP path: it is a third `ffmpeg` that decodes, throttles to a
//! configured frame rate, downscales, and writes `rawvideo` (rgb24 / nv12 /
//! yuv420p) to a unix-socket / fifo sink the vision engine consumes. A crash or
//! stall on this leg is contained — the orchestrator restarts it on its own
//! ladder and the encode + radio path is untouched.
//!
//! Two modes:
//! - **decoupled tap (default):** [`vision_tap_args`] builds the third-ffmpeg
//!   `rtsp://localhost:8554/main → rawvideo → sink` command. This is the only
//!   path that ever spawns a process; it cannot perturb the encoder.
//! - **pre-encode split (opt-in, `raw_tap`):** the encoder command grows a
//!   `-filter_complex` split with a second `rawvideo` output to the sink, while
//!   the existing encode/RTSP output bytes are left exactly as the builder
//!   produced them. See [`augment_encoder_with_raw_tap`] in [`crate::encoder`].
//!   No new process; lower latency at the cost of riding inside the
//!   parity-critical encoder command, which is why it is gated off by default.
//!
//! Process-liveness alone is never proof of work here either: the third ffmpeg
//! can hold the sink open while pushing nothing, so the orchestrator drives the
//! same `-progress` output-counter watchdog the wfb tap uses (the
//! [`crate::wfb_tee::ProgressTracker`] primitives are reused — they are
//! consumer-agnostic).

use crate::process::ManagedProcess;

/// Default frame-tap output-progress watchdog window. Mirrors the wfb-tap
/// floor: an RTSP DESCRIBE/SETUP/PLAY handshake plus the first decoded frame
/// can take several seconds on a cold bench, so a tighter threshold would trip
/// false-positive restarts during install + reload races.
pub use crate::wfb_tee::WFB_TEE_PROGRESS_TIMEOUT as VISION_TAP_PROGRESS_TIMEOUT;

/// Build the exact `ffmpeg` arg vector for the decoupled vision frame tap.
///
/// Returns the program (`ffmpeg`) followed by its arguments. Unlike the wfb
/// tap this leg DECODES (no `-c:v copy`): it throttles the stream to `fps`,
/// scales to `width`x`height`, forces the requested raw pixel format, and emits
/// a headerless `rawvideo` stream to `sink` (a unix socket or fifo the vision
/// engine reads). Flags:
/// - `-rtsp_transport tcp` pulls the local stream over loopback TCP (no UDP
///   frag of large keyframe NALs);
/// - `-fflags nobuffer -flags low_delay` keep the input demuxer from buffering;
/// - `-vf fps=<fps>,scale=<w>:<h>` drops to the target rate and resizes BEFORE
///   the pixel-format conversion so the engine gets exactly the geometry it
///   asked for at the rate it can keep up with;
/// - `-pix_fmt <format>` lands rgb24 / nv12 / yuv420p planar bytes;
/// - `-f rawvideo` emits headerless frames (the engine knows w/h/format from
///   its config, matching the shared frame descriptor);
/// - `-progress pipe:2` forces the periodic status report to stderr so the
///   output-counter watchdog has a token to count.
pub fn vision_tap_args(
    rtsp_in: &str,
    fps: u32,
    width: u32,
    height: u32,
    pixel_format: &str,
    sink: &str,
) -> Vec<String> {
    // Floor fps at 1 so a misconfigured 0 cannot produce a degenerate filter.
    let fps = fps.max(1);
    vec![
        "-fflags".into(),
        "nobuffer".into(),
        "-flags".into(),
        "low_delay".into(),
        "-rtsp_transport".into(),
        "tcp".into(),
        "-i".into(),
        rtsp_in.to_string(),
        "-an".into(),
        "-vf".into(),
        format!("fps={fps},scale={width}:{height}"),
        "-pix_fmt".into(),
        pixel_format.to_string(),
        "-f".into(),
        "rawvideo".into(),
        "-progress".into(),
        "pipe:2".into(),
        sink.to_string(),
    ]
}

/// The local mediamtx RTSP source URL the tap reads from (the same `/main`
/// stream every other consumer reads).
pub fn local_rtsp_url(rtsp_port: u16) -> String {
    format!("rtsp://localhost:{rtsp_port}/main")
}

/// Spawn the decoupled vision tap ffmpeg through [`ManagedProcess`]
/// (setsid/killpg owner — no second spawner). The caller composes the source
/// URL from the live mediamtx RTSP port.
///
/// Best-effort by contract: a spawn failure leaves the encode + wfb path fully
/// up. The orphan sweep for a straggler from a prior crashed run is the
/// caller's job ([`crate::process::kill_orphans`] with [`orphan_pattern`]).
pub fn spawn_vision_tap(
    rtsp_port: u16,
    fps: u32,
    width: u32,
    height: u32,
    pixel_format: &str,
    sink: &str,
) -> std::io::Result<ManagedProcess> {
    let rtsp_in = local_rtsp_url(rtsp_port);
    let args = vision_tap_args(&rtsp_in, fps, width, height, pixel_format, sink);
    ManagedProcess::spawn("vision_tap", "ffmpeg", &args)
}

/// The pattern [`crate::process::kill_orphans`] sweeps before a (re)spawn: any
/// stray vision-tap ffmpeg whose command line targets this sink, so a stale
/// reader from a prior crashed run cannot hold the socket against the fresh
/// one.
pub fn orphan_pattern(sink: &str) -> String {
    sink.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn vision_tap_args_decoupled_shape() {
        let rtsp_in = local_rtsp_url(8554);
        assert_eq!(rtsp_in, "rtsp://localhost:8554/main");

        let got = vision_tap_args(
            &rtsp_in,
            10,
            640,
            480,
            "rgb24",
            "/run/ados/vision-tap-main.sock",
        );
        let expected: Vec<String> = [
            "-fflags",
            "nobuffer",
            "-flags",
            "low_delay",
            "-rtsp_transport",
            "tcp",
            "-i",
            "rtsp://localhost:8554/main",
            "-an",
            "-vf",
            "fps=10,scale=640:480",
            "-pix_fmt",
            "rgb24",
            "-f",
            "rawvideo",
            "-progress",
            "pipe:2",
            "/run/ados/vision-tap-main.sock",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn vision_tap_args_honours_format_and_geometry() {
        let got = vision_tap_args(
            "rtsp://localhost:8554/main",
            5,
            1280,
            720,
            "nv12",
            "/run/ados/v.sock",
        );
        let vf = got.iter().position(|t| t == "-vf").unwrap();
        assert_eq!(got[vf + 1], "fps=5,scale=1280:720");
        let pf = got.iter().position(|t| t == "-pix_fmt").unwrap();
        assert_eq!(got[pf + 1], "nv12");
        // rawvideo, never a copy: a decode is mandatory for the vision engine.
        assert!(got.iter().any(|t| t == "rawvideo"));
        assert!(!got.iter().any(|t| t == "copy"));
        // The sink is the last token.
        assert_eq!(got.last().unwrap(), "/run/ados/v.sock");
    }

    #[test]
    fn vision_tap_args_floors_fps_at_one() {
        let got = vision_tap_args("rtsp://localhost:8554/main", 0, 640, 480, "rgb24", "/s");
        let vf = got.iter().position(|t| t == "-vf").unwrap();
        assert_eq!(got[vf + 1], "fps=1,scale=640:480");
    }

    #[test]
    fn orphan_pattern_is_the_sink() {
        assert_eq!(
            orphan_pattern("/run/ados/vision-tap-main.sock"),
            "/run/ados/vision-tap-main.sock"
        );
    }

    #[test]
    fn progress_timeout_matches_wfb_floor() {
        assert_eq!(VISION_TAP_PROGRESS_TIMEOUT, Duration::from_secs(15));
    }
}
