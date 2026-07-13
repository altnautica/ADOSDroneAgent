//! The streaming perception-offload session (node side).
//!
//! An NPU-less drone opens a session and streams its camera to this node; the
//! node runs the detector over every frame and streams detections back. Unlike
//! a one-shot [`crate::ComputeJobKind::PerceptionOffload`] job (one frame per
//! job), a session is continuous: [`run_offload_session`] pulls frames from an
//! [`OffloadFrameStream`] and emits an [`OffloadDetectionBatch`] per frame onto a
//! channel the daemon forwards to the session's WS subscribers.
//!
//! Two frame sources share the trait: [`RtspFrameStream`] pulls the drone's live
//! RTSP feed (the real path — the drone already publishes `rtsp://…:8554/main`);
//! [`VecFrameStream`] replays a fixed set of frames for tests + the SITL gate.
//! Inference runs on a blocking thread so a slow model never starves the async
//! runtime.

use std::process::Stdio;
use std::time::{Duration, Instant};

use ados_protocol::offload::{Detection, FrameRef, OffloadDetectionBatch};
use anyhow::{anyhow, Result};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

use crate::offload::Detector;

/// Reconnect backoff for a continuous (live) frame source: the first retry waits
/// this long after a transient frame error.
const RECONNECT_BACKOFF_BASE_MS: u64 = 200;
/// The backoff doubles after each failed retry, up to this cap.
const RECONNECT_BACKOFF_CAP_MS: u64 = 2_000;
/// Give up (end the session so the drone re-opens it) only once a live source has
/// stayed broken continuously for this long. A single blip never ends a session.
const RECONNECT_GIVE_UP_MS: u128 = 30_000;

/// One decoded RGB24 frame streamed from a drone, plus the metadata a detection
/// is tied back to. `pixels` is `width * height * 3` bytes, row-major.
#[derive(Debug, Clone)]
pub struct OffloadFrame {
    pub camera_id: String,
    pub ts_ms: i64,
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// A source of RGB24 frames for an offload session. `next_frame` resolves with
/// the next frame or an error when the source ends (stream closed, capture
/// process exited); the session then stops (the drone re-opens it).
///
/// The returned future is `Send` so a session over an arbitrary stream can be
/// spawned onto the multi-threaded runtime (the session manager spawns one per
/// streaming session); both concrete impls' futures are already `Send`.
pub trait OffloadFrameStream: Send {
    fn next_frame(&mut self) -> impl std::future::Future<Output = Result<OffloadFrame>> + Send;

    /// Whether this is a live, reconnectable source. A live source (the drone's
    /// RTSP feed) treats a frame error as a transient hiccup to retry, not the end
    /// of the session; a finite replay source treats exhaustion as terminal.
    /// Defaults to `false` so a replay source ends when it runs out.
    fn is_continuous(&self) -> bool {
        false
    }
}

/// Run a streaming offload session: pull frames, run the detector on each on a
/// blocking thread, and emit one [`OffloadDetectionBatch`] per frame onto `sink`.
///
/// Ends when the stream ends, the detector fails, `cancel` is notified, or the
/// sink is dropped (the last subscriber left). `session_id` and `camera_id` tag
/// every emitted batch; `seq` counts frames within the session. A per-frame
/// detector error stops the session (it is a backend fault, not bad data) rather
/// than silently emitting empty batches forever.
pub async fn run_offload_session<S: OffloadFrameStream>(
    session_id: &str,
    camera_id: &str,
    mut stream: S,
    detector: Arc<dyn Detector>,
    sink: tokio::sync::mpsc::Sender<OffloadDetectionBatch>,
    cancel: Arc<tokio::sync::Notify>,
) {
    let mut seq: u64 = 0;
    // Reconnect state for a continuous (live) source: a transient frame error is a
    // hiccup to retry with a bounded backoff, not the end of the session. Reset on
    // every good frame.
    let mut backoff_ms = RECONNECT_BACKOFF_BASE_MS;
    let mut failing_since: Option<Instant> = None;
    // Hold ONE notified future for the whole session so a cancel is never missed
    // between iterations (a fresh `cancel.notified()` per loop can race with the
    // signal). The canceller uses `notify_one`, so a cancel fired before the first
    // poll is stored as a permit and observed on the first select.
    let cancelled = cancel.notified();
    tokio::pin!(cancelled);
    loop {
        let next = tokio::select! {
            f = stream.next_frame() => f,
            _ = &mut cancelled => break,
        };
        let frame = match next {
            Ok(f) => {
                // A good frame clears the reconnect state.
                backoff_ms = RECONNECT_BACKOFF_BASE_MS;
                failing_since = None;
                f
            }
            Err(e) => {
                if !stream.is_continuous() {
                    // A finite/replay source: exhaustion is terminal.
                    tracing::info!(session = session_id, error = %e, "offload session frame stream ended");
                    break;
                }
                // A live source (the drone's RTSP feed): a transient blip (an
                // ffmpeg exit / RTSP drop) is a hiccup, not the end. The stream
                // respawns its capture on the next call, so back off and retry;
                // give up only if it stays broken past the budget (then the session
                // ends and the drone re-opens it).
                let since = *failing_since.get_or_insert_with(Instant::now);
                if since.elapsed().as_millis() > RECONNECT_GIVE_UP_MS {
                    tracing::warn!(session = session_id, error = %e, "offload frame stream down past the reconnect budget; ending session");
                    break;
                }
                tracing::info!(session = session_id, error = %e, backoff_ms, "offload frame stream hiccup; reconnecting");
                // Cancel-aware backoff: a cancel during the wait still stops promptly.
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(backoff_ms)) => {}
                    _ = &mut cancelled => break,
                }
                backoff_ms = backoff_ms.saturating_mul(2).min(RECONNECT_BACKOFF_CAP_MS);
                continue;
            }
        };

        let detections = match run_detector(&detector, &frame).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(session = session_id, error = %e, "offload session detector failed; stopping");
                break;
            }
        };

        let batch = OffloadDetectionBatch::new(
            session_id,
            camera_id,
            seq,
            frame.ts_ms,
            frame.width,
            frame.height,
            detections,
        );
        seq = seq.wrapping_add(1);

        // A closed channel means every subscriber left; the session is done.
        if sink.send(batch).await.is_err() {
            tracing::info!(
                session = session_id,
                "offload session sink closed; stopping"
            );
            break;
        }
    }
}

/// Run the detector on one frame on a blocking thread (a real model can take
/// tens of ms; the async runtime must not block on it). Clones the small handle
/// + frame ref and moves the pixels in; only the detections come back.
async fn run_detector(
    detector: &Arc<dyn Detector>,
    frame: &OffloadFrame,
) -> Result<Vec<Detection>> {
    let detector = detector.clone();
    let frame_ref = FrameRef {
        camera_id: frame.camera_id.clone(),
        width: frame.width,
        height: frame.height,
        ts_ms: frame.ts_ms,
    };
    let pixels = frame.pixels.clone();
    tokio::task::spawn_blocking(move || detector.infer(&frame_ref, Some(&pixels)))
        .await
        .map_err(|e| anyhow!("detector task join: {e}"))?
        .map_err(|e| anyhow!("detector: {e}"))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Pulls decoded RGB24 frames from a drone's RTSP feed via `ffmpeg`.
///
/// Spawns `ffmpeg -rtsp_transport tcp -i <url> -pix_fmt rgb24 -f rawvideo -` and
/// reads fixed-size frames off its stdout. The drone advertises its camera size
/// in the session params, so each frame is a known `width * height * 3` bytes —
/// the same fixed-frame read the local capture source uses. The real cross-host
/// transport for the offload path.
pub struct RtspFrameStream {
    camera_id: String,
    url: String,
    width: u32,
    height: u32,
    frame_bytes: usize,
    child: Option<Child>,
}

impl RtspFrameStream {
    /// A stream for `url` (e.g. `rtsp://drone.local:8554/main`) decoding to the
    /// drone's advertised `width` x `height` RGB24.
    pub fn new(
        camera_id: impl Into<String>,
        url: impl Into<String>,
        width: u32,
        height: u32,
    ) -> Self {
        Self {
            camera_id: camera_id.into(),
            url: url.into(),
            width,
            height,
            frame_bytes: width as usize * height as usize * 3,
            child: None,
        }
    }

    fn spawn(&self) -> Result<Child> {
        let child = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-rtsp_transport",
                "tcp",
                "-i",
                &self.url,
                "-pix_fmt",
                "rgb24",
                "-f",
                "rawvideo",
                "-",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        Ok(child)
    }

    async fn ensure_spawned(&mut self) -> Result<()> {
        if self.child.is_none() {
            self.child = Some(self.spawn()?);
        }
        Ok(())
    }
}

impl OffloadFrameStream for RtspFrameStream {
    async fn next_frame(&mut self) -> Result<OffloadFrame> {
        self.ensure_spawned().await?;
        let child = self.child.as_mut().expect("spawned above");
        let stdout = child
            .stdout
            .as_mut()
            .ok_or_else(|| anyhow!("ffmpeg rtsp child has no stdout"))?;
        let mut pixels = vec![0u8; self.frame_bytes];
        if let Err(e) = stdout.read_exact(&mut pixels).await {
            // ffmpeg exited or the stream dropped; drop the child so a re-open
            // respawns rather than reading a half frame from a dead pipe.
            self.child = None;
            return Err(e.into());
        }
        Ok(OffloadFrame {
            camera_id: self.camera_id.clone(),
            ts_ms: now_ms(),
            width: self.width,
            height: self.height,
            pixels,
        })
    }

    // The RTSP feed is a live source: a read error is a transient blip to
    // reconnect through, not the end of the session.
    fn is_continuous(&self) -> bool {
        true
    }
}

/// Replays a fixed set of frames, then ends. The synthetic source for tests + the
/// SITL offload gate: no ffmpeg, no camera, no RTSP — deterministic frames in,
/// detections out.
pub struct VecFrameStream {
    frames: std::collections::VecDeque<OffloadFrame>,
}

impl VecFrameStream {
    pub fn new(frames: Vec<OffloadFrame>) -> Self {
        Self {
            frames: frames.into(),
        }
    }

    /// `count` solid-grey frames of `width` x `height`, timestamped `t0 + i`.
    pub fn solid(camera_id: &str, width: u32, height: u32, count: usize, t0: i64) -> Self {
        let frames = (0..count)
            .map(|i| OffloadFrame {
                camera_id: camera_id.to_string(),
                ts_ms: t0 + i as i64,
                width,
                height,
                pixels: vec![128u8; width as usize * height as usize * 3],
            })
            .collect();
        Self::new(frames)
    }
}

impl OffloadFrameStream for VecFrameStream {
    async fn next_frame(&mut self) -> Result<OffloadFrame> {
        self.frames
            .pop_front()
            .ok_or_else(|| anyhow!("vec frame stream exhausted"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MockDetector;

    #[tokio::test]
    async fn a_session_emits_one_batch_per_frame() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let cancel = Arc::new(tokio::sync::Notify::new());
        let stream = VecFrameStream::solid("front", 64, 48, 3, 1000);
        let detector: Arc<dyn Detector> = Arc::new(MockDetector);
        let session = tokio::spawn(async move {
            run_offload_session("sess-1", "front", stream, detector, tx, cancel).await;
        });

        let mut batches = Vec::new();
        while let Some(b) = rx.recv().await {
            batches.push(b);
        }
        session.await.unwrap();

        assert_eq!(batches.len(), 3, "one batch per frame");
        for (i, b) in batches.iter().enumerate() {
            assert_eq!(b.seq, i as u64, "seq counts frames");
            assert_eq!(b.session_id, "sess-1");
            assert_eq!(b.camera_id, "front");
            assert_eq!(b.width, 64);
            assert_eq!(b.height, 48);
            // The mock returns one centered box per frame.
            assert_eq!(b.detections.len(), 1);
        }
        // ts_ms rides the frame's timestamp.
        assert_eq!(batches[0].ts_ms, 1000);
        assert_eq!(batches[2].ts_ms, 1002);
    }

    #[tokio::test]
    async fn a_dropped_sink_stops_the_session() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let cancel = Arc::new(tokio::sync::Notify::new());
        // Many frames, but the receiver is dropped after the first: the session
        // must stop rather than spin forever on a closed channel.
        let stream = VecFrameStream::solid("front", 16, 16, 1000, 0);
        let detector: Arc<dyn Detector> = Arc::new(MockDetector);
        drop(rx);
        // A tiny wait so the drop lands, then run: the first send fails, stopping.
        let session = tokio::spawn(async move {
            run_offload_session("s", "front", stream, detector, tx, cancel).await;
        });
        // Bounded: if the stop logic regressed this would hang the test.
        tokio::time::timeout(std::time::Duration::from_secs(5), session)
            .await
            .expect("session stopped on the closed sink")
            .unwrap();
    }

    #[tokio::test]
    async fn a_continuous_stream_reconnects_after_a_transient_hiccup() {
        // A live source that errors ONCE (an ffmpeg / RTSP blip) then yields frames.
        // The session must NOT end on the blip: it reconnects and resumes emitting.
        struct FlakyThenSteady {
            errored: bool,
            camera_id: String,
        }
        impl OffloadFrameStream for FlakyThenSteady {
            async fn next_frame(&mut self) -> Result<OffloadFrame> {
                if !self.errored {
                    self.errored = true;
                    return Err(anyhow!("transient rtsp blip"));
                }
                Ok(OffloadFrame {
                    camera_id: self.camera_id.clone(),
                    ts_ms: 1,
                    width: 16,
                    height: 16,
                    pixels: vec![128u8; 16 * 16 * 3],
                })
            }
            fn is_continuous(&self) -> bool {
                true
            }
        }

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let cancel = Arc::new(tokio::sync::Notify::new());
        let stream = FlakyThenSteady {
            errored: false,
            camera_id: "front".into(),
        };
        let detector: Arc<dyn Detector> = Arc::new(MockDetector);
        let c = cancel.clone();
        let session = tokio::spawn(async move {
            run_offload_session("s-flaky", "front", stream, detector, tx, c).await;
        });

        // Despite the first-frame error, the session reconnects (a short backoff)
        // and emits batches once frames flow again; seq starts at the first GOOD
        // frame (the errored read emitted nothing).
        let b0 = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("a batch arrives after the reconnect")
            .expect("channel open");
        assert_eq!(b0.session_id, "s-flaky");
        assert_eq!(
            b0.seq, 0,
            "the errored read emitted nothing; seq starts at the first good frame"
        );
        let b1 = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("the session keeps streaming")
            .expect("channel open");
        assert_eq!(b1.seq, 1);

        // Stop the (otherwise endless) session.
        cancel.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), session)
            .await
            .expect("session stopped on cancel")
            .unwrap();
    }

    #[tokio::test]
    async fn cancel_stops_a_running_session() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1024);
        let cancel = Arc::new(tokio::sync::Notify::new());
        let stream = VecFrameStream::solid("front", 16, 16, 100_000, 0);
        let detector: Arc<dyn Detector> = Arc::new(MockDetector);
        let c = cancel.clone();
        let session = tokio::spawn(async move {
            run_offload_session("s", "front", stream, detector, tx, c).await;
        });
        cancel.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), session)
            .await
            .expect("session stopped on cancel")
            .unwrap();
    }
}
