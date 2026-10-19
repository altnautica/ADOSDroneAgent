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
//! The tap is lazy: the serving socket is bound up front, but the decoding
//! ffmpeg is spawned only once an engine connects, and stopped again after the
//! engine has been gone for [`ENGINE_IDLE_STOP`] (see [`crate::tap_lazy`]). A
//! node with vision enabled and no engine attached decodes nothing.
//!
//! Process-liveness alone is never proof of work here either: the third ffmpeg
//! can hold the pipe open while pushing nothing, so a session with an engine
//! attached and no frame for [`VISION_TAP_PROGRESS_TIMEOUT`] ends as
//! [`TapSessionEnd::Stalled`] and the decoder is restarted.

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

/// Host CLOCK_MONOTONIC in nanoseconds, the clock the tap header carries.
pub fn mono_ns() -> u64 {
    use nix::time::{clock_gettime, ClockId};
    clock_gettime(ClockId::CLOCK_MONOTONIC)
        .map(|t| t.tv_sec() as u64 * 1_000_000_000 + t.tv_nsec() as u64)
        .unwrap_or(0)
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

/// Why one tap session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapSessionEnd {
    /// ffmpeg's stdout hit EOF: the source process exited.
    SourceEnded,
    /// The listener failed: the leg must be rebuilt from a fresh bind.
    ListenerFailed,
    /// No engine for the idle window: the decoder can be stopped.
    EngineIdle,
    /// An engine is attached but no frame arrived for the progress window: the
    /// decoder is alive and pushing nothing.
    Stalled,
}

/// How long the decoder keeps running after the last engine disconnects.
pub const ENGINE_IDLE_STOP: Duration = Duration::from_secs(30);

/// Serve one decoder run to the connected vision engine (Contract F): read
/// fixed-size headerless frames off ffmpeg's stdout, prepend the frozen
/// `ADVT` header, and write them to the engine.
///
/// - A dedicated reader task owns `stdout` so `read_exact` (not cancel-safe)
///   never races the accept loop; it publishes the latest frame on a `watch`
///   channel (newest-frame-wins, so a slow engine transparently drops frames
///   rather than back-pressuring ffmpeg).
/// - A fresh connection always replaces the current engine. A write error or
///   an [`ENGINE_WRITE_TIMEOUT`] drops that engine — one bad consumer never
///   wedges the tap.
/// - With `idle` set, the session ends [`TapSessionEnd::EngineIdle`] once no
///   engine has been attached for that long.
pub async fn serve_tap_session<R>(
    listener: &UnixListener,
    mut engine: Option<tokio::net::UnixStream>,
    stdout: R,
    format: FrameFormat,
    width: u32,
    height: u32,
    idle: Option<Duration>,
) -> TapSessionEnd
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let frame_size = format.frame_bytes(width, height);
    if frame_size == 0 {
        tracing::error!(
            width,
            height,
            "vision_tap_zero_frame_size; session not started"
        );
        return TapSessionEnd::SourceEnded;
    }

    let (tx, mut rx) = watch::channel::<Option<(u64, Arc<Vec<u8>>)>>(None);

    // Reader owns ffmpeg stdout exclusively. Each frame is read into its own
    // buffer and MOVED into the channel — no per-frame copy — with the host
    // monotonic arrival time stamped as soon as the read completes. The buffer
    // the channel just released is reclaimed for the next read whenever the
    // server has finished with it.
    let reader = tokio::spawn(async move {
        let mut stdout = stdout;
        let mut spare: Option<Vec<u8>> = None;
        loop {
            let mut buf = spare.take().unwrap_or_else(|| vec![0u8; frame_size]);
            if let Err(e) = stdout.read_exact(&mut buf).await {
                tracing::info!(error = %e, "vision_tap_source_eof");
                return;
            }
            let arrival_ns = mono_ns();
            let previous = tx.send_replace(Some((arrival_ns, Arc::new(buf))));
            if tx.is_closed() {
                return; // server gone
            }
            spare = previous.and_then(|(_, frame)| Arc::try_unwrap(frame).ok());
        }
    });

    let now = tokio::time::Instant::now();
    let mut engine_gone_at = if engine.is_some() { None } else { Some(now) };
    let mut last_frame_at = now;
    let end = loop {
        let idle_deadline = match (idle, engine_gone_at) {
            (Some(window), Some(gone)) => Some(gone + window),
            _ => None,
        };
        let stall_deadline = engine
            .as_ref()
            .map(|_| last_frame_at + VISION_TAP_PROGRESS_TIMEOUT);
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        tracing::info!("vision_tap_engine_connected");
                        engine = Some(stream);
                        engine_gone_at = None;
                        // A fresh engine gets the full progress window.
                        last_frame_at = tokio::time::Instant::now();
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "vision_tap_accept_failed");
                        break TapSessionEnd::ListenerFailed;
                    }
                }
            }
            changed = rx.changed() => {
                if changed.is_err() {
                    break TapSessionEnd::SourceEnded;
                }
                last_frame_at = tokio::time::Instant::now();
                let frame = rx.borrow_and_update().clone();
                let Some((ts_mono_ns, frame)) = frame else { continue };
                if let Some(e) = engine.as_mut() {
                    let failed = match timeout(
                        ENGINE_WRITE_TIMEOUT,
                        write_tap_frame(e, format, width, height, &frame, ts_mono_ns),
                    )
                    .await
                    {
                        Ok(Ok(())) => false,
                        Ok(Err(err)) => {
                            tracing::warn!(error = %err, "vision_tap_engine_write_failed; dropping engine");
                            true
                        }
                        Err(_) => {
                            tracing::warn!("vision_tap_engine_write_timeout; dropping engine");
                            true
                        }
                    };
                    if failed {
                        engine = None;
                        engine_gone_at = Some(tokio::time::Instant::now());
                    }
                }
            }
            _ = sleep_until_opt(idle_deadline) => {
                tracing::info!("vision_tap_engine_idle; stopping decoder");
                break TapSessionEnd::EngineIdle;
            }
            _ = sleep_until_opt(stall_deadline) => {
                tracing::warn!(
                    threshold_s = VISION_TAP_PROGRESS_TIMEOUT.as_secs(),
                    "vision_tap_zombie_detected: engine attached but no frames"
                );
                break TapSessionEnd::Stalled;
            }
        }
    };
    reader.abort();
    end
}

/// Sleep until `deadline`, or forever when there is none.
async fn sleep_until_opt(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending::<()>().await,
    }
}

/// Serve a decoder's frames on `listener` until its stdout ends or the
/// listener fails, with no idle stop. The lazy leg ([`crate::tap_lazy`]) runs
/// [`serve_tap_session`] directly; this form serves a single fixed source.
pub async fn run_vision_tap_server<R>(
    listener: UnixListener,
    stdout: R,
    format: FrameFormat,
    width: u32,
    height: u32,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    let _ = serve_tap_session(&listener, None, stdout, format, width, height, None).await;
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
    fn mono_ns_advances() {
        let a = mono_ns();
        let b = mono_ns();
        assert!(a > 0 && b >= a);
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

        let dir = tempfile::tempdir().unwrap();
        let sink = dir.path().join("vision-tap.sock");
        let sink_str = sink.to_string_lossy().to_string();

        let (w, h) = (4u32, 2u32);
        let format = FrameFormat::Rgb24;
        let frame_size = format.frame_bytes(w, h); // 24 bytes

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
        let hdr = decode_tap_header(&header).unwrap();
        assert_eq!(hdr.format, format);
        assert_eq!(hdr.width, w);
        assert_eq!(hdr.height, h);
        assert_eq!(hdr.byte_len, frame_size);
        assert!(hdr.ts_mono_ns > 0, "writer stamps host monotonic arrival");
        let mut pixels = vec![0u8; hdr.byte_len];
        engine.read_exact(&mut pixels).await.unwrap();
        assert_eq!(pixels, frame_a);

        server.abort();
    }
}
