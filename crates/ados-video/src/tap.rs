//! Vision frame tap: an additive, optional leg that feeds raw decoded frames
//! to the on-box vision engine over the frozen tap contract
//! ([`ados_protocol::tap`], Contract F).
//!
//! The tap is a SEPARATE consumer of the same local mediamtx RTSP `/main`
//! stream the wfb radio fan-out reads. It never alters the encode output or the
//! `wfb_tee` RTP path: it is a third `ffmpeg` that decodes, throttles to a
//! configured frame rate, downscales, and emits headerless `rawvideo` (rgb24 /
//! nv12 / yuv420p) to its **stdout pipe**. A small in-process **reframer**
//! ([`run_vision_tap_server`]) reads those fixed-size frames, prepends the
//! 16-byte `ADVT` header, and serves them on a unix socket the vision engine
//! connects to as a client. A crash or stall on this leg is contained — the
//! orchestrator restarts it on its own ladder and the encode + radio path is
//! untouched.
//!
//! The engine is the client ([`ados_vision`]'s `TapSource` does
//! `UnixStream::connect`), so this side binds and serves. The framing is
//! defined once in [`ados_protocol::tap`] and both halves build against it —
//! the same frozen-wire-contract discipline the mavlink, state, and plugin
//! sockets already follow, now extended to the vision seam.
//!
//! Two modes:
//! - **decoupled tap (default):** [`vision_tap_args`] builds the third-ffmpeg
//!   `rtsp://localhost:8554/main → rawvideo → stdout` command; the reframer
//!   serves the socket. This is the only path that ever spawns a process; it
//!   cannot perturb the encoder.
//! - **pre-encode split (opt-in, `raw_tap`):** the encoder command grows a
//!   `-filter_complex` split with a second `rawvideo` output. NOTE: that leg is
//!   not yet ADVT-framed (it predates Contract F); it is off by default and the
//!   decoupled tap is the supported path to the engine.
//!
//! Process-liveness alone is never proof of work here either: the third ffmpeg
//! can hold the pipe open while pushing nothing, so the orchestrator drives the
//! same `-progress` output-counter watchdog the wfb tap uses (the
//! [`crate::wfb_tee::ProgressTracker`] primitives are reused — they are
//! consumer-agnostic).

use std::sync::Arc;
use std::time::Duration;

use ados_protocol::framebus::FrameFormat;
use ados_protocol::tap::write_tap_frame;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::net::UnixListener;
use tokio::sync::watch;
use tokio::time::timeout;

use crate::process::ManagedProcess;

/// Default frame-tap output-progress watchdog window. Mirrors the wfb-tap
/// floor: an RTSP DESCRIBE/SETUP/PLAY handshake plus the first decoded frame
/// can take several seconds on a cold bench, so a tighter threshold would trip
/// false-positive restarts during install + reload races.
pub use crate::wfb_tee::WFB_TEE_PROGRESS_TIMEOUT as VISION_TAP_PROGRESS_TIMEOUT;

/// Upper bound on a single engine write. A hung engine must not stall the whole
/// tap (which would back-pressure ffmpeg into a false zombie restart): on a
/// timeout the reframer drops that engine and keeps draining ffmpeg.
const ENGINE_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Build the exact `ffmpeg` arg vector for the decoupled vision frame tap.
///
/// Returns the arguments (the program is `ffmpeg`). Unlike the wfb tap this leg
/// DECODES (no `-c:v copy`): it throttles the stream to `fps`, scales to
/// `width`x`height`, forces the requested raw pixel format, and emits headerless
/// `rawvideo` to **stdout** (`pipe:1`) for the in-process reframer to header +
/// serve. Flags:
/// - `-rtsp_transport tcp` pulls the local stream over loopback TCP (no UDP
///   frag of large keyframe NALs);
/// - `-fflags nobuffer -flags low_delay` keep the input demuxer from buffering;
/// - `-vf fps=<fps>,scale=<w>:<h>` drops to the target rate and resizes BEFORE
///   the pixel-format conversion so the reframer gets exactly the geometry the
///   engine asked for at the rate it can keep up with;
/// - `-pix_fmt <format>` lands rgb24 / nv12 / yuv420p planar bytes;
/// - `-f rawvideo pipe:1` emits fixed-size headerless frames to stdout (the
///   reframer sizes each frame from w/h/format, then prepends the ADVT header);
/// - `-progress pipe:2` forces the periodic status report to stderr so the
///   output-counter watchdog has a token to count.
pub fn vision_tap_args(
    rtsp_in: &str,
    fps: u32,
    width: u32,
    height: u32,
    pixel_format: &str,
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
        "pipe:1".into(),
    ]
}

/// The local mediamtx RTSP source URL the tap reads from (the same `/main`
/// stream every other consumer reads).
pub fn local_rtsp_url(rtsp_port: u16) -> String {
    format!("rtsp://localhost:{rtsp_port}/main")
}

/// The [`FrameFormat`] for a config pixel-format string, defaulting to rgb24 on
/// an unrecognised value (matching `VisionTapConfig::pixel_format`).
pub fn frame_format_from_str(pixel_format: &str) -> FrameFormat {
    match pixel_format {
        "nv12" => FrameFormat::Nv12,
        "yuv420p" => FrameFormat::Yuv420p,
        _ => FrameFormat::Rgb24,
    }
}

/// Size in bytes of one decoded `rawvideo` frame the reframer reads off ffmpeg's
/// stdout. rgb24 is 3 bytes/px; nv12 and yuv420p are 4:2:0 planar at 12 bits/px
/// (= 3/2 bytes/px, exact for even geometry, which is the only valid 4:2:0 case).
pub fn frame_size_bytes(format: FrameFormat, width: u32, height: u32) -> usize {
    let px = width as usize * height as usize;
    match format {
        FrameFormat::Rgb24 => px * 3,
        FrameFormat::Nv12 | FrameFormat::Yuv420p => px * 3 / 2,
    }
}

/// Spawn the decoupled vision tap ffmpeg through [`ManagedProcess`]
/// (setsid/killpg owner — no second spawner), capturing stdout so the reframer
/// can read the raw frames. The caller composes the source URL from the live
/// mediamtx RTSP port.
///
/// Best-effort by contract: a spawn failure leaves the encode + wfb path fully
/// up. Take stdout with [`ManagedProcess::take_stdout`] and hand it to
/// [`run_vision_tap_server`].
pub fn spawn_vision_tap(
    rtsp_port: u16,
    fps: u32,
    width: u32,
    height: u32,
    pixel_format: &str,
) -> std::io::Result<ManagedProcess> {
    let rtsp_in = local_rtsp_url(rtsp_port);
    let args = vision_tap_args(&rtsp_in, fps, width, height, pixel_format);
    ManagedProcess::spawn_capturing_stdout("vision_tap", "ffmpeg", &args)
}

/// Bind the tap's serving socket, removing any stale socket file first so a
/// leftover from a hard crash cannot make the bind fail with `EADDRINUSE`.
pub fn bind_vision_tap(sink: &str) -> std::io::Result<UnixListener> {
    // A stale socket inode from a prior run is the only thing at this path (the
    // reframer is the sole writer); remove it best-effort before binding.
    let _ = std::fs::remove_file(sink);
    UnixListener::bind(sink)
}

/// The reframer: read fixed-size headerless frames off ffmpeg's stdout, prepend
/// the frozen `ADVT` header, and serve them on `listener` to the connected
/// vision engine (Contract F). Runs until ffmpeg's stdout hits EOF (ffmpeg
/// exited — the orchestrator's process-exit watchdog then restarts the whole
/// leg); the caller aborts this task on stop.
///
/// - A dedicated reader task owns `stdout` so `read_exact` (not cancel-safe)
///   never races the accept loop; it publishes the latest frame on a `watch`
///   channel (newest-frame-wins, so a slow engine transparently drops frames
///   rather than back-pressuring ffmpeg).
/// - The server loop accepts the engine, forwards each new frame, and on a write
///   error or a [`ENGINE_WRITE_TIMEOUT`] drops that engine and re-accepts — one
///   bad consumer never wedges the tap.
pub async fn run_vision_tap_server<R>(
    listener: UnixListener,
    stdout: R,
    format: FrameFormat,
    width: u32,
    height: u32,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    let frame_size = frame_size_bytes(format, width, height);
    if frame_size == 0 {
        tracing::error!(
            width,
            height,
            "vision_tap_zero_frame_size; reframer not started"
        );
        return;
    }

    let (tx, mut rx) = watch::channel::<Option<Arc<Vec<u8>>>>(None);

    // Reader owns ffmpeg stdout exclusively. read_exact is safe here because it
    // is the only awaiter of the stream.
    let reader = tokio::spawn(async move {
        let mut stdout = stdout;
        let mut buf = vec![0u8; frame_size];
        loop {
            match stdout.read_exact(&mut buf).await {
                Ok(_) => {
                    // Newest-frame-wins: a slow engine simply misses frames.
                    if tx.send(Some(Arc::new(buf.clone()))).is_err() {
                        return; // server gone
                    }
                }
                Err(e) => {
                    tracing::info!(error = %e, "vision_tap_source_eof");
                    return;
                }
            }
        }
    });

    // One engine at a time (the vision engine is the only consumer). Accepting
    // and forwarding share a select so a fresh connection ALWAYS wins immediately:
    // when the engine restarts it reconnects here and replaces the stale stream
    // (dropping the old one closes it), instead of being starved in the listener
    // backlog until the dead connection's write finally fails.
    let mut engine: Option<tokio::net::UnixStream> = None;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        tracing::info!("vision_tap_engine_connected");
                        engine = Some(stream);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "vision_tap_accept_failed");
                        // A broken listener is fatal; bail so the leg restarts clean.
                        break;
                    }
                }
            }
            changed = rx.changed() => {
                if changed.is_err() {
                    // Reader ended (ffmpeg EOF) — the whole leg is done.
                    break;
                }
                let frame = rx.borrow_and_update().clone();
                let Some(frame) = frame else { continue };
                if let Some(e) = engine.as_mut() {
                    match timeout(
                        ENGINE_WRITE_TIMEOUT,
                        write_tap_frame(e, format, width, height, &frame),
                    )
                    .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(err)) => {
                            tracing::warn!(error = %err, "vision_tap_engine_write_failed; dropping engine");
                            engine = None;
                        }
                        Err(_) => {
                            tracing::warn!("vision_tap_engine_write_timeout; dropping engine");
                            engine = None;
                        }
                    }
                }
            }
        }
    }

    reader.abort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::tap::{decode_tap_header, TAP_HEADER_LEN};
    use tokio::net::UnixStream as ClientStream;

    #[test]
    fn vision_tap_args_decoupled_shape() {
        let rtsp_in = local_rtsp_url(8554);
        assert_eq!(rtsp_in, "rtsp://localhost:8554/main");

        let got = vision_tap_args(&rtsp_in, 10, 640, 480, "rgb24");
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
            "pipe:1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn vision_tap_args_honours_format_and_geometry() {
        let got = vision_tap_args("rtsp://localhost:8554/main", 5, 1280, 720, "nv12");
        let vf = got.iter().position(|t| t == "-vf").unwrap();
        assert_eq!(got[vf + 1], "fps=5,scale=1280:720");
        let pf = got.iter().position(|t| t == "-pix_fmt").unwrap();
        assert_eq!(got[pf + 1], "nv12");
        // rawvideo, never a copy: a decode is mandatory for the vision engine.
        assert!(got.iter().any(|t| t == "rawvideo"));
        assert!(!got.iter().any(|t| t == "copy"));
        // Output is stdout, so the reframer can header + serve it.
        assert_eq!(got.last().unwrap(), "pipe:1");
    }

    #[test]
    fn vision_tap_args_floors_fps_at_one() {
        let got = vision_tap_args("rtsp://localhost:8554/main", 0, 640, 480, "rgb24");
        let vf = got.iter().position(|t| t == "-vf").unwrap();
        assert_eq!(got[vf + 1], "fps=1,scale=640:480");
    }

    #[test]
    fn frame_size_matches_pixel_format() {
        assert_eq!(
            frame_size_bytes(FrameFormat::Rgb24, 640, 480),
            640 * 480 * 3
        );
        assert_eq!(
            frame_size_bytes(FrameFormat::Nv12, 640, 480),
            640 * 480 * 3 / 2
        );
        assert_eq!(
            frame_size_bytes(FrameFormat::Yuv420p, 1280, 720),
            1280 * 720 * 3 / 2
        );
    }

    #[test]
    fn format_string_maps_with_rgb24_fallback() {
        assert_eq!(frame_format_from_str("rgb24"), FrameFormat::Rgb24);
        assert_eq!(frame_format_from_str("nv12"), FrameFormat::Nv12);
        assert_eq!(frame_format_from_str("yuv420p"), FrameFormat::Yuv420p);
        assert_eq!(frame_format_from_str("bogus"), FrameFormat::Rgb24);
    }

    #[test]
    fn progress_timeout_matches_wfb_floor() {
        assert_eq!(VISION_TAP_PROGRESS_TIMEOUT, Duration::from_secs(15));
    }

    /// The whole reframer seam: a fake ffmpeg stdout (an in-memory duplex pipe we
    /// feed fixed-size frames into) → the reframer → a connected engine client
    /// reads ADVT-headered frames back with the exact pixel bytes.
    #[tokio::test]
    async fn reframer_serves_advt_frames_to_a_connecting_engine() {
        use tokio::io::AsyncReadExt as _;
        use tokio::io::AsyncWriteExt as _;

        let dir = std::env::temp_dir().join(format!("ados-tap-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sink = dir.join("vision-tap.sock");
        let sink_str = sink.to_string_lossy().to_string();

        let (w, h) = (4u32, 2u32);
        let format = FrameFormat::Rgb24;
        let frame_size = frame_size_bytes(format, w, h); // 24 bytes

        // Fake ffmpeg stdout: an in-memory duplex pipe. We write raw frames into
        // `feed`; the reframer reads them off `ffmpeg_stdout` (an AsyncRead).
        let (mut feed, ffmpeg_stdout) = tokio::io::duplex(4096);

        let listener = bind_vision_tap(&sink_str).unwrap();
        let server = tokio::spawn(run_vision_tap_server(listener, ffmpeg_stdout, format, w, h));

        // Engine connects as a client and reads one ADVT frame.
        let mut engine = ClientStream::connect(&sink_str).await.unwrap();

        // Feed several frames of known bytes; the engine should read at least one.
        let frame_a: Vec<u8> = (0..frame_size as u8).collect();
        for _ in 0..8 {
            feed.write_all(&frame_a).await.unwrap();
            feed.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let mut header = [0u8; TAP_HEADER_LEN];
        engine.read_exact(&mut header).await.unwrap();
        let (df, dw, dh, dl) = decode_tap_header(&header).unwrap();
        assert_eq!(df, format);
        assert_eq!(dw, w);
        assert_eq!(dh, h);
        assert_eq!(dl, frame_size);
        let mut pixels = vec![0u8; dl];
        engine.read_exact(&mut pixels).await.unwrap();
        assert_eq!(pixels, frame_a);

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
