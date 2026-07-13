//! The streaming perception-offload session (node side).
//!
//! An NPU-less drone opens a session and streams its camera to this node; the
//! node runs the detector and streams detections back. Unlike a one-shot
//! [`crate::ComputeJobKind::PerceptionOffload`] job (one frame per job), a session
//! is continuous: [`run_offload_session`] pulls frames from an
//! [`OffloadFrameStream`] and emits an [`OffloadDetectionBatch`] onto a channel the
//! daemon forwards to the session's WS subscribers.
//!
//! A live source can decode frames faster than the detector infers. Rather than
//! run the model over a growing backlog (so detections trail the moving scene),
//! the live path decouples ingestion from inference: a background reader keeps
//! publishing the NEWEST decoded frame into a depth-1 channel, and the consumer
//! always infers on the freshest one, dropping any frames it was too slow for.
//! Detections track the scene instead of lagging behind it. A finite/replay
//! source keeps exact sequential semantics (one batch per frame, in order).
//!
//! Two frame sources share the trait: [`RtspFrameStream`] pulls the drone's live
//! RTSP feed (the real path — the drone already publishes `rtsp://…:8554/main`)
//! and drives the process-latest path; [`VecFrameStream`] replays a fixed set of
//! frames for tests + the SITL gate and drives the sequential path. Inference runs
//! on a blocking thread so a slow model never starves the async runtime.

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
    /// of the session, and is fed through the process-latest path; a finite replay
    /// source treats exhaustion as terminal and runs sequentially. Defaults to
    /// `false` so a replay source ends when it runs out.
    fn is_continuous(&self) -> bool {
        false
    }
}

/// Run a streaming offload session: pull frames, run the detector, and emit one
/// [`OffloadDetectionBatch`] per processed frame onto `sink`.
///
/// A live source (`is_continuous()`) runs the process-latest path: it always
/// infers on the newest available frame and drops any stale ones queued behind a
/// slow detector, so detections track the moving scene. A finite/replay source
/// runs sequentially: one batch per frame, in order.
///
/// Ends when the stream ends, the detector fails, `cancel` is notified, or the
/// sink is dropped (the last subscriber left). `session_id` and `camera_id` tag
/// every emitted batch; `seq` counts processed frames within the session. A
/// per-frame detector error stops the session (it is a backend fault, not bad
/// data) rather than silently emitting empty batches forever.
pub async fn run_offload_session<S: OffloadFrameStream + 'static>(
    session_id: &str,
    camera_id: &str,
    stream: S,
    detector: Arc<dyn Detector>,
    sink: tokio::sync::mpsc::Sender<OffloadDetectionBatch>,
    cancel: Arc<tokio::sync::Notify>,
) {
    if stream.is_continuous() {
        run_process_latest_session(session_id, camera_id, stream, detector, sink, cancel).await;
    } else {
        run_sequential_session(session_id, camera_id, stream, detector, sink, cancel).await;
    }
}

/// The finite/replay path: pull frames in order, infer on each, emit one batch
/// per frame. Any frame error (typically exhaustion) ends the session — a replay
/// source never reconnects. Used by tests + the SITL gate.
async fn run_sequential_session<S: OffloadFrameStream>(
    session_id: &str,
    camera_id: &str,
    mut stream: S,
    detector: Arc<dyn Detector>,
    sink: tokio::sync::mpsc::Sender<OffloadDetectionBatch>,
    cancel: Arc<tokio::sync::Notify>,
) {
    let mut seq: u64 = 0;
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
            Ok(f) => f,
            Err(e) => {
                // A finite/replay source: exhaustion is terminal.
                tracing::info!(session = session_id, error = %e, "offload session frame stream ended");
                break;
            }
        };
        match detect_and_emit(session_id, camera_id, seq, frame, &detector, &sink).await {
            Step::Continue => seq = seq.wrapping_add(1),
            Step::Stop => break,
        }
    }
}

/// The live path: decouple ingestion from inference so the detector always gets
/// the freshest frame. A background reader pulls frames as fast as the source
/// yields them and publishes only the newest into a depth-1 [`tokio::sync::watch`]
/// channel; this consumer waits for a new latest-frame, infers on it, and emits.
/// Any frames the reader published while the detector was busy are overwritten and
/// never inferred — the stale backlog is dropped instead of trailing the scene.
async fn run_process_latest_session<S: OffloadFrameStream + 'static>(
    session_id: &str,
    camera_id: &str,
    stream: S,
    detector: Arc<dyn Detector>,
    sink: tokio::sync::mpsc::Sender<OffloadDetectionBatch>,
    cancel: Arc<tokio::sync::Notify>,
) {
    // The reader publishes the newest decoded frame here; a frame the consumer
    // never observed is simply overwritten (depth-1 conflate). `None` until the
    // first frame arrives.
    let (latest_tx, mut latest_rx) = tokio::sync::watch::channel::<Option<OffloadFrame>>(None);

    // A dedicated stop signal for the reader. The consumer owns the external
    // `cancel` (whose `notify_one` wakes exactly one waiter), so the reader gets
    // its own token the consumer trips when the session stops for any reason.
    let reader_stop = Arc::new(tokio::sync::Notify::new());
    let reader_task = {
        let session_owned = session_id.to_string();
        let reader_stop = reader_stop.clone();
        tokio::spawn(async move {
            run_latest_reader(&session_owned, stream, latest_tx, reader_stop).await;
        })
    };

    let mut seq: u64 = 0;
    // One notified future for the whole session so an external cancel is never
    // missed between iterations (mirrors the sequential path).
    let cancelled = cancel.notified();
    tokio::pin!(cancelled);
    loop {
        // Wait for a fresh newest-frame. `changed()` erroring means the reader
        // dropped its sender — the live source ended past the reconnect budget.
        tokio::select! {
            changed = latest_rx.changed() => {
                if changed.is_err() {
                    break;
                }
            }
            _ = &mut cancelled => break,
        }
        // Take the newest available frame; anything the reader published while we
        // were inferring collapses to this one value. `borrow_and_update` marks it
        // seen so the next `changed()` waits for a genuinely newer frame.
        let Some(frame) = latest_rx.borrow_and_update().clone() else {
            continue;
        };

        match detect_and_emit(session_id, camera_id, seq, frame, &detector, &sink).await {
            Step::Continue => seq = seq.wrapping_add(1),
            Step::Stop => break,
        }
    }

    // Stop the reader (it may be blocked in a frame read) and join it so the
    // capture child is torn down before the session task returns.
    reader_stop.notify_one();
    let _ = reader_task.await;
}

/// The ingestion half of the process-latest path: continuously pull frames from a
/// live source and publish only the NEWEST into `latest_tx`, so the consumer
/// always infers on the freshest frame and the ones it was too slow for are
/// dropped. A transient frame error (an ffmpeg exit / RTSP drop) is reconnected
/// through with a bounded backoff; the reader ends the session only after the
/// source stays broken past the give-up budget (by dropping `latest_tx`, which
/// the consumer observes) or when `stop` is tripped.
async fn run_latest_reader<S: OffloadFrameStream>(
    session_id: &str,
    mut stream: S,
    latest_tx: tokio::sync::watch::Sender<Option<OffloadFrame>>,
    stop: Arc<tokio::sync::Notify>,
) {
    let mut backoff_ms = RECONNECT_BACKOFF_BASE_MS;
    let mut failing_since: Option<Instant> = None;
    let stopped = stop.notified();
    tokio::pin!(stopped);
    loop {
        let next = tokio::select! {
            f = stream.next_frame() => f,
            _ = &mut stopped => break,
        };
        match next {
            Ok(frame) => {
                // A good frame clears the reconnect state and becomes the newest;
                // an unobserved previous frame is simply overwritten.
                backoff_ms = RECONNECT_BACKOFF_BASE_MS;
                failing_since = None;
                if latest_tx.send(Some(frame)).is_err() {
                    // The consumer dropped its receiver (the session ended); stop.
                    break;
                }
                // Yield after each publish so a source that decodes frames faster
                // than real time (or an immediate synthetic source) never starves
                // the inferring consumer on a single-threaded runtime.
                tokio::task::yield_now().await;
            }
            Err(e) => {
                // A live source: a transient blip is a hiccup to reconnect through,
                // not the end. The stream respawns its capture on the next call, so
                // back off and retry; give up only if it stays broken past the
                // budget (then the session ends and the drone re-opens it).
                let since = *failing_since.get_or_insert_with(Instant::now);
                if since.elapsed().as_millis() > RECONNECT_GIVE_UP_MS {
                    tracing::warn!(session = session_id, error = %e, "offload frame stream down past the reconnect budget; ending session");
                    break;
                }
                tracing::info!(session = session_id, error = %e, backoff_ms, "offload frame stream hiccup; reconnecting");
                // Cancel-aware backoff: a stop during the wait still ends promptly.
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(backoff_ms)) => {}
                    _ = &mut stopped => break,
                }
                backoff_ms = backoff_ms.saturating_mul(2).min(RECONNECT_BACKOFF_CAP_MS);
            }
        }
    }
    // Returning drops `latest_tx`, which signals the consumer the source ended.
}

/// Whether the session should keep going after a frame is processed.
enum Step {
    Continue,
    Stop,
}

/// Run the detector on `frame`, tag the batch with `seq`, and push it onto `sink`.
/// Returns [`Step::Stop`] when the detector failed (a backend fault) or the sink
/// closed (the last subscriber left) — both end the session.
async fn detect_and_emit(
    session_id: &str,
    camera_id: &str,
    seq: u64,
    frame: OffloadFrame,
    detector: &Arc<dyn Detector>,
    sink: &tokio::sync::mpsc::Sender<OffloadDetectionBatch>,
) -> Step {
    // Read the batch metadata off the frame before its pixels move into inference.
    let (ts_ms, width, height) = (frame.ts_ms, frame.width, frame.height);
    let detections = match run_detector(detector, frame).await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(session = session_id, error = %e, "offload session detector failed; stopping");
            return Step::Stop;
        }
    };

    let batch =
        OffloadDetectionBatch::new(session_id, camera_id, seq, ts_ms, width, height, detections);

    // A closed channel means every subscriber left; the session is done.
    if sink.send(batch).await.is_err() {
        tracing::info!(
            session = session_id,
            "offload session sink closed; stopping"
        );
        return Step::Stop;
    }
    Step::Continue
}

/// Run the detector on one frame on a blocking thread (a real model can take tens
/// of ms; the async runtime must not block on it). Consumes the frame so its
/// pixels move into the blocking task with no copy; only the detections come back.
async fn run_detector(detector: &Arc<dyn Detector>, frame: OffloadFrame) -> Result<Vec<Detection>> {
    let detector = detector.clone();
    let frame_ref = FrameRef {
        camera_id: frame.camera_id,
        width: frame.width,
        height: frame.height,
        ts_ms: frame.ts_ms,
    };
    let pixels = frame.pixels;
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
/// Spawns `ffmpeg -rtsp_transport tcp -flags low_delay -i <url> -pix_fmt rgb24 -f
/// rawvideo -` and reads fixed-size frames off its stdout. The drone advertises
/// its camera size in the session params, so each frame is a known
/// `width * height * 3` bytes — the same fixed-frame read the local capture source
/// uses. The real cross-host transport for the offload path.
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
                // Decoder low-delay so a frame is emitted as soon as it decodes
                // rather than waiting to fill a reorder window. (No `-fflags
                // nobuffer` — it triggers early-EOF reconnect churn on this source.)
                "-flags",
                "low_delay",
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
    use crate::{ComputeError, MockDetector};

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

    #[tokio::test]
    async fn process_latest_drops_stale_frames_on_a_continuous_source() {
        // A live source that decodes a burst of frames far faster than the detector
        // infers, then stops producing. The process-latest path must always infer
        // on the NEWEST published frame, dropping the ones queued behind a slow
        // detector — so the consumer processes far fewer frames than were emitted
        // and its last inference is on the freshest frame, never a stale one.
        const N: u64 = 64;

        struct CountingContinuous {
            next: u64,
            limit: u64,
            camera_id: String,
        }
        impl OffloadFrameStream for CountingContinuous {
            async fn next_frame(&mut self) -> Result<OffloadFrame> {
                if self.next >= self.limit {
                    // Burst exhausted: never yield another frame so the last one
                    // published stays the newest for the slow consumer to settle on.
                    return std::future::pending().await;
                }
                let ts = self.next as i64;
                self.next += 1;
                Ok(OffloadFrame {
                    camera_id: self.camera_id.clone(),
                    ts_ms: ts,
                    width: 8,
                    height: 8,
                    pixels: vec![0u8; 8 * 8 * 3],
                })
            }
            fn is_continuous(&self) -> bool {
                true
            }
        }

        // A detector far slower than the (immediate) reader, so the reader always
        // races ahead and the consumer only ever catches the newest frame.
        struct SlowMock;
        impl Detector for SlowMock {
            fn name(&self) -> &str {
                "slow-mock"
            }
            fn infer(
                &self,
                frame: &FrameRef,
                _pixels: Option<&[u8]>,
            ) -> std::result::Result<Vec<Detection>, ComputeError> {
                std::thread::sleep(Duration::from_millis(15));
                Ok(vec![Detection {
                    bbox: [0.4, 0.4, 0.2, 0.2],
                    class: "object".into(),
                    confidence: 0.9,
                    track_id: Some(frame.ts_ms.unsigned_abs() % 1000),
                }])
            }
        }

        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let cancel = Arc::new(tokio::sync::Notify::new());
        let stream = CountingContinuous {
            next: 0,
            limit: N,
            camera_id: "front".into(),
        };
        let detector: Arc<dyn Detector> = Arc::new(SlowMock);
        let c = cancel.clone();
        let session = tokio::spawn(async move {
            run_offload_session("s-latest", "front", stream, detector, tx, c).await;
        });

        // Collect batches until the source stops producing (no batch for a beat =
        // the consumer is parked on the newest frame with nothing newer to process).
        let mut batches = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_millis(400), rx.recv()).await {
                Ok(Some(b)) => batches.push(b),
                Ok(None) => break, // sink closed
                Err(_) => break,   // quiet: parked on the newest frame
            }
        }

        assert!(!batches.is_empty(), "the consumer inferred at least once");
        // Conflation: the detector is far slower than the reader, so most of the
        // burst never reached inference — stale frames were dropped.
        assert!(
            (batches.len() as u64) < N,
            "stale frames were dropped ({} of {} frames inferred)",
            batches.len(),
            N
        );
        // The consumer caught the freshest frame the reader published, not a stale
        // queued one.
        assert_eq!(
            batches.last().unwrap().ts_ms,
            (N - 1) as i64,
            "the last inference is on the newest frame"
        );
        // Each batch is on a strictly newer frame than the previous — intermediate
        // frames were skipped, never re-processed or regressed.
        for w in batches.windows(2) {
            assert!(
                w[1].ts_ms > w[0].ts_ms,
                "frames advance, never repeat or regress"
            );
        }

        // End the (otherwise parked) session.
        cancel.notify_one();
        tokio::time::timeout(Duration::from_secs(5), session)
            .await
            .expect("session stops on cancel")
            .unwrap();
    }

    #[tokio::test]
    async fn process_latest_reconnects_after_transient_reader_errors() {
        // A live source that errors on its first two reads (RTSP blips) then yields
        // a bounded burst before parking. The process-latest reader must retry
        // through the errors so the consumer still receives detections.
        struct FlakyThenCounting {
            reads: u64,
            camera_id: String,
        }
        impl OffloadFrameStream for FlakyThenCounting {
            async fn next_frame(&mut self) -> Result<OffloadFrame> {
                self.reads += 1;
                if self.reads <= 2 {
                    return Err(anyhow!("transient rtsp blip"));
                }
                if self.reads > 6 {
                    // Stop producing so the consumer settles rather than spinning.
                    return std::future::pending().await;
                }
                Ok(OffloadFrame {
                    camera_id: self.camera_id.clone(),
                    ts_ms: self.reads as i64,
                    width: 8,
                    height: 8,
                    pixels: vec![0u8; 8 * 8 * 3],
                })
            }
            fn is_continuous(&self) -> bool {
                true
            }
        }

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let cancel = Arc::new(tokio::sync::Notify::new());
        let stream = FlakyThenCounting {
            reads: 0,
            camera_id: "front".into(),
        };
        let detector: Arc<dyn Detector> = Arc::new(MockDetector);
        let c = cancel.clone();
        let session = tokio::spawn(async move {
            run_offload_session("s-reconnect", "front", stream, detector, tx, c).await;
        });

        // Despite the first two errored reads, a detection arrives once frames flow
        // (the reader reconnected through the blips). It rides one of the good
        // frames (read 3 onward); which one depends on how far the reader raced
        // ahead of the slow-to-start consumer, so assert it is a real good frame.
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("a batch arrives after the reader reconnects")
            .expect("channel open");
        assert_eq!(first.session_id, "s-reconnect");
        assert_eq!(
            first.seq, 0,
            "the errored reads emitted nothing; seq starts at the first processed frame"
        );
        assert!(
            first.ts_ms >= 3,
            "the first batch rides a good frame past the two errored reads (ts {})",
            first.ts_ms
        );

        cancel.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), session)
            .await
            .expect("session stops on cancel")
            .unwrap();
    }
}
