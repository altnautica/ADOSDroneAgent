//! Node-side streaming-session manager: the auto-start + supervise half of the
//! offload path.
//!
//! A one-shot [`crate::ComputeJobKind::PerceptionOffload`] job runs the detector
//! over ONE frame carried in its params. A *streaming session* is different: an
//! NPU-less drone opens a continuous frames→detections lane, streaming its live
//! camera (RTSP) to the node and subscribing to the node's per-session detection
//! WebSocket ([`crate::offload_ws`]). This manager owns that lane on the node.
//!
//! The trigger is still a `PerceptionOffload` job, but one whose params carry a
//! `session` block ([`SessionSpec::from_job_params`]) instead of (or as well as)
//! a single `frame`. The daemon's worker loop, on claiming such a job, hands the
//! spec here — [`OffloadSessionManager::start`] registers a
//! [`crate::StreamingSession`] record and spawns a *supervisor* around
//! [`crate::run_offload_session`]. The supervisor pumps emitted batches into the
//! shared [`DetectionBroadcaster`] the WS router fans out, and it treats a
//! streaming session as a first-class, supervised, restartable unit — not a
//! fire-and-forget task.
//!
//! Session vs. job: a streaming session runs on its own tokio task, OUTSIDE the
//! batch worker pool that drains reconstruction jobs. So a session never occupies
//! a worker slot and never queues behind a reconstruction — batch cannot starve
//! streaming of a worker slot. The one shared resource is raw CPU/GPU; the worker
//! yields to a live session before starting a heavy backend
//! ([`OffloadSessionManager::should_yield_to_streaming`]), but fine-grained
//! CPU/GPU preemption of an in-flight reconstruction is out of scope.
//!
//! Lifecycle: sessions are keyed by id, so a re-submit of a live session is a
//! no-op (idempotent), and a session self-removes from the registry when it ends.
//! A session that dies from a backend fault or a panic is restarted a bounded
//! number of times before it is given up cleanly (never silently dropped, never
//! restart-looping). The registry is the source of truth for `/api/compute/status`
//! (`active_sessions` + a state breakdown) and `/api/compute/sessions` (the live
//! records). [`OffloadSessionManager::stop_all`] tears every session down on
//! shutdown.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::{mpsc, Notify};

use crate::offload::Detector;
use crate::offload_stream::{
    run_offload_session, OffloadFrameStream, RtspFrameStream, SessionExit,
};
use crate::offload_ws::{pump_to_broadcaster, DetectionBroadcaster};
use crate::session_registry::{now_ms, SessionProgress, SessionRegistry, WorkPriority};

/// The default RGB24 frame size assumed when a session spec omits it. The drone
/// normally advertises its camera size in the spec; this is the safety default
/// so a spec missing it still names a concrete fixed-frame size for the decoder.
const DEFAULT_WIDTH: u32 = 1280;
const DEFAULT_HEIGHT: u32 = 720;
/// Per-session channel + broadcaster buffer depth (batches). A slow WS subscriber
/// past the buffer lags and skips (never blocking the detector); the pump keeps
/// pace with the detector on this bound.
const SESSION_CHANNEL_CAP: usize = 64;

/// How many times a faulting session is restarted within [`RESTART_WINDOW`] before
/// it is given up cleanly (Closed). Bounds a persistently-failing session so it
/// never restart-loops forever.
const MAX_RESTARTS: u32 = 3;
/// The rolling window the restart budget is counted over. A fault after a quiet
/// stretch (window elapsed) resets the budget, so an occasional transient fault
/// never permanently exhausts a session's restarts.
const RESTART_WINDOW: Duration = Duration::from_secs(60);
/// The first restart waits this long; it doubles per consecutive restart, capped
/// at [`RESTART_BACKOFF_CAP_MS`], so a tight fault loop backs off between attempts.
const RESTART_BACKOFF_BASE_MS: u64 = 100;
const RESTART_BACKOFF_CAP_MS: u64 = 2_000;
/// How often the stall watchdog scans for `Live` sessions that went quiet.
const STALL_WATCHDOG_TICK: Duration = Duration::from_secs(1);

fn default_width() -> u32 {
    DEFAULT_WIDTH
}
fn default_height() -> u32 {
    DEFAULT_HEIGHT
}

/// The description of a streaming offload session, lifted from a job's params.
///
/// The wire shape is `params.session = { id, rtsp_url, camera_id, width?, height? }`.
/// `id` is the session id (the WS path + the batch tag), `rtsp_url` is the drone's
/// live feed the node pulls (e.g. `rtsp://drone.local:8554/main`), `camera_id`
/// tags each detection, and `width`/`height` size the decoded RGB24 frames.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SessionSpec {
    pub id: String,
    pub rtsp_url: String,
    pub camera_id: String,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
}

impl SessionSpec {
    /// Extract a session spec from a job's `params`, or `None` when the job
    /// carries no `session` block (a one-shot per-frame offload job, which takes
    /// the unchanged path). A present-but-malformed `session` is treated as
    /// absent — the caller falls back to the per-frame path rather than failing.
    pub fn from_job_params(params: &serde_json::Value) -> Option<Self> {
        let session = params.get("session")?;
        match serde_json::from_value::<SessionSpec>(session.clone()) {
            Ok(spec) if !spec.id.is_empty() && !spec.rtsp_url.is_empty() => Some(spec),
            _ => None,
        }
    }
}

/// Owns the node's streaming-session registry + the shared detection broadcaster
/// the WS router fans out. Held by the daemon for its lifetime.
pub struct OffloadSessionManager {
    broadcaster: Arc<DetectionBroadcaster>,
    detector: Arc<dyn Detector>,
    /// Whether the node serves offload sessions at all (the
    /// `perception.serving.enabled` toggle). When false, `start` declines so the
    /// node reconstructs but never runs perception offload for a drone.
    serve_offload: bool,
    /// The live streaming-session registry — the source of truth for every
    /// session's state, throughput, and reconnect / restart history, plus the
    /// lock-free `active_sessions` projection shared into the engine's heartbeat.
    registry: Arc<SessionRegistry>,
}

impl OffloadSessionManager {
    /// The priority a live streaming offload session runs at.
    pub const STREAMING_PRIORITY: WorkPriority = WorkPriority::Streaming;
    /// The priority best-effort batch reconstruction runs at.
    pub const BATCH_PRIORITY: WorkPriority = WorkPriority::Batch;
    /// The restart budget (exposed for tests + observability).
    pub const MAX_RESTARTS: u32 = MAX_RESTARTS;

    /// A manager fanning sessions out over `broadcaster`, running `detector` on
    /// every streamed frame. `serve_offload` is the config toggle: when false,
    /// every `start` is declined (the node still does reconstruction).
    pub fn new(
        broadcaster: Arc<DetectionBroadcaster>,
        detector: Arc<dyn Detector>,
        serve_offload: bool,
    ) -> Self {
        Self {
            broadcaster,
            detector,
            serve_offload,
            registry: Arc::new(SessionRegistry::new()),
        }
    }

    /// The live-session projection (Opening|Live count), shared into the engine so
    /// its heartbeat reflects active offload sessions with no lock.
    pub fn session_counter(&self) -> Arc<std::sync::atomic::AtomicU32> {
        self.registry.active_counter()
    }

    /// The session registry handle, so the job API can read the live records
    /// (`/api/compute/sessions`) + the state breakdown (`/api/compute/status`).
    pub fn registry(&self) -> Arc<SessionRegistry> {
        self.registry.clone()
    }

    /// Whether any streaming session is live (the batch-yields-to-streaming
    /// signal).
    pub fn any_live(&self) -> bool {
        self.registry.any_live()
    }

    /// Whether best-effort batch reconstruction should yield to a live streaming
    /// session before starting its heavy backend. Streaming is latency-critical
    /// ([`Self::STREAMING_PRIORITY`]) and outranks best-effort batch
    /// ([`Self::BATCH_PRIORITY`]); sessions run outside the batch worker pool, so
    /// this only nudges shared-CPU/GPU scheduling, never a worker slot.
    pub fn should_yield_to_streaming(&self) -> bool {
        self.any_live() && Self::STREAMING_PRIORITY > Self::BATCH_PRIORITY
    }

    /// Start (or re-affirm) the session described by `spec`, pulling the drone's
    /// RTSP feed. Idempotent: a spec whose id is already live is a no-op. The
    /// session is supervised — a backend fault / panic restarts it under a bounded
    /// budget over fresh RTSP streams.
    pub async fn start(&self, spec: SessionSpec) {
        let (cam, url, w, h) = (
            spec.camera_id.clone(),
            spec.rtsp_url.clone(),
            spec.width,
            spec.height,
        );
        // A fresh RTSP stream per (re)start: a supervised restart re-spawns ffmpeg.
        let factory = move || Some(RtspFrameStream::new(cam.clone(), url.clone(), w, h));
        self.start_with_factory(spec, factory).await;
    }

    /// Start a session over a single injected frame stream. Tests call this with a
    /// synthetic [`crate::VecFrameStream`] so the manager's lifecycle is exercised
    /// with no ffmpeg / camera / RTSP. The single stream is consumed once; a
    /// supervised restart (a backend fault) has no fresh stream to re-run, so the
    /// session closes rather than restarting.
    pub async fn start_with_stream<S>(&self, spec: SessionSpec, stream: S)
    where
        S: OffloadFrameStream + 'static,
    {
        let mut held = Some(stream);
        self.start_with_factory(spec, move || held.take()).await;
    }

    /// The core supervised start: register the session, spawn the detection pump,
    /// and spawn the supervisor around the runner. `factory` yields a fresh frame
    /// stream per (re)start; `None` means no more streams to run (a consumed
    /// single-shot stream), so the supervisor closes rather than restarting.
    pub async fn start_with_factory<S, F>(&self, spec: SessionSpec, factory: F)
    where
        S: OffloadFrameStream + 'static,
        F: FnMut() -> Option<S> + Send + 'static,
    {
        if !self.serve_offload {
            tracing::info!(session = %spec.id, "offload serving disabled by config; session declined");
            return;
        }
        // Register the session (Opening) synchronously up front, so the active
        // count reflects it before the supervisor task is even scheduled and a
        // concurrent re-submit of the same id is deduped.
        let cancel = Arc::new(Notify::new());
        if !self.registry.register(
            &spec.id,
            &spec.camera_id,
            &spec.rtsp_url,
            cancel.clone(),
            now_ms(),
        ) {
            tracing::info!(session = %spec.id, "offload session already running; re-submit ignored");
            return;
        }

        // The detection return lane: the runner emits one batch per frame onto
        // `tx`; the pump forwards them into the broadcaster the WS router fans out.
        // The SUPERVISOR owns `tx` and hands a clone to each runner iteration, so a
        // restart is transparent to the drone's WS subscriber — the pump (and the
        // WS with it) closes only when the session truly ends and the supervisor
        // drops its `tx`.
        let (tx, rx) = mpsc::channel(SESSION_CHANNEL_CAP);
        tokio::spawn(pump_to_broadcaster(
            spec.id.clone(),
            rx,
            self.broadcaster.clone(),
        ));

        let detector = self.detector.clone();
        let registry = self.registry.clone();
        tokio::spawn(supervise_session(
            spec, factory, detector, tx, cancel, registry,
        ));
    }

    /// Stop the named session (best-effort). Returns whether it was live. The
    /// session's supervisor observes the cancel, closes the session, and removes
    /// it from the registry as it unwinds.
    pub async fn stop(&self, session_id: &str) -> bool {
        match self.registry.cancel_of(session_id) {
            Some(c) => {
                c.notify_one();
                true
            }
            None => false,
        }
    }

    /// Cancel every live session (daemon shutdown). Each supervisor reaps its own
    /// registry record as it stops.
    pub async fn stop_all(&self) {
        for c in self.registry.all_cancels() {
            c.notify_one();
        }
    }

    /// How many sessions are currently live (any of Opening/Live/Stalled).
    pub async fn active_count(&self) -> usize {
        self.registry.len()
    }

    /// The shared broadcaster (the WS router mounts on it).
    pub fn broadcaster(&self) -> Arc<DetectionBroadcaster> {
        self.broadcaster.clone()
    }

    /// Spawn the stall watchdog: on a tick, flip any `Live` session with no batch
    /// within the stall window to `Stalled`, so a session whose source went quiet
    /// without a reader error reads honestly rather than as a stale `Live`. The
    /// daemon calls this once at startup; it runs for the process lifetime.
    pub fn spawn_stall_watchdog(&self) {
        let registry = self.registry.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(STALL_WATCHDOG_TICK);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                for id in registry
                    .stall_quiet_sessions(now_ms(), crate::session_registry::STALL_WINDOW_MS)
                {
                    tracing::info!(session = %id, "offload session stalled: no detections within the stall window");
                }
            }
        });
    }
}

/// The outer task-level supervisor around one streaming session. The runner has
/// its own inner frame-reconnect (an RTSP blip is retried); this supervises the
/// runner TASK: a backend fault or a panic restarts the session under a bounded
/// budget over fresh streams; a clean end (cancel / stream ended / sink closed /
/// source lost) closes it. On close, the session is removed from the registry and
/// `tx` is dropped, ending the pump (and the drone's WS subscriber).
async fn supervise_session<S, F>(
    spec: SessionSpec,
    mut factory: F,
    detector: Arc<dyn Detector>,
    tx: mpsc::Sender<ados_protocol::offload::OffloadDetectionBatch>,
    cancel: Arc<Notify>,
    registry: Arc<SessionRegistry>,
) where
    S: OffloadFrameStream + 'static,
    F: FnMut() -> Option<S> + Send,
{
    let progress = SessionProgress::bound(registry.clone(), spec.id.clone());
    let mut restarts: u32 = 0;
    let mut window_start = Instant::now();

    tracing::info!(session = %spec.id, camera = %spec.camera_id, "offload session started");

    let reason: &str = loop {
        let Some(stream) = factory() else {
            break "frame source exhausted";
        };

        // Run the session as a CHILD task so a panic is catchable — a panic is a
        // fault to restart under budget, never a silent loss of the session.
        let runner = {
            let id = spec.id.clone();
            let cam = spec.camera_id.clone();
            let det = detector.clone();
            let sink = tx.clone();
            let rc = cancel.clone();
            let prog = progress.clone();
            tokio::spawn(async move {
                run_offload_session(&id, &cam, stream, det, sink, rc, prog).await
            })
        };

        match runner.await {
            Ok(SessionExit::Cancelled) => break "cancelled",
            Ok(SessionExit::StreamEnded) => break "frame stream ended",
            Ok(SessionExit::SinkClosed) => break "detection sink closed",
            Ok(SessionExit::SourceLost) => break "source lost past the reconnect budget",
            Ok(SessionExit::BackendFault) => {
                if !restart_allowed(&mut restarts, &mut window_start) {
                    break "gave up after repeated backend faults";
                }
                registry.note_restart(&spec.id);
                tracing::warn!(session = %spec.id, restarts, "offload session backend faulted; restarting");
                if backoff_or_cancel(restarts, &cancel).await {
                    break "cancelled";
                }
            }
            Err(join_err) => {
                // The runner task panicked (a bug, not a handled fault). Restart it
                // under the same budget rather than silently losing the session.
                if !restart_allowed(&mut restarts, &mut window_start) {
                    break "gave up after repeated task panics";
                }
                registry.note_restart(&spec.id);
                tracing::warn!(session = %spec.id, restarts, error = %join_err, "offload session task panicked; restarting");
                if backoff_or_cancel(restarts, &cancel).await {
                    break "cancelled";
                }
            }
        }
    };

    registry.close(&spec.id);
    tracing::info!(session = %spec.id, reason, "offload session closed");
    // `tx` drops here → the pump's channel closes → the drone's WS subscribers
    // close (the drone re-opens if it still wants the session).
    drop(tx);
}

/// Whether a fault may be restarted: reset the budget if the rolling window has
/// elapsed since it started, then allow up to [`MAX_RESTARTS`] within the window.
/// Returns `false` (give up) once the budget is exhausted.
fn restart_allowed(restarts: &mut u32, window_start: &mut Instant) -> bool {
    if window_start.elapsed() >= RESTART_WINDOW {
        *restarts = 0;
        *window_start = Instant::now();
    }
    if *restarts >= MAX_RESTARTS {
        return false;
    }
    *restarts += 1;
    true
}

/// Back off before a restart (escalating, capped), cancellable. Returns `true`
/// when a cancel landed during the wait (the session should stop, not restart).
async fn backoff_or_cancel(restarts: u32, cancel: &Notify) -> bool {
    let backoff = RESTART_BACKOFF_BASE_MS
        .saturating_mul(1u64 << restarts.saturating_sub(1).min(5))
        .min(RESTART_BACKOFF_CAP_MS);
    // During the backoff no runner is running, so this is the only waiter on
    // `cancel`; a `notify_one` fired here (or a stored permit) is observed.
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(backoff)) => false,
        _ = cancel.notified() => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::offload_ws::DetectionBroadcaster;
    use crate::{ComputeError, Detection, Detector, FrameRef, MockDetector, VecFrameStream};
    use std::sync::atomic::{AtomicU32, Ordering};

    fn manager() -> OffloadSessionManager {
        OffloadSessionManager::new(
            Arc::new(DetectionBroadcaster::new(64)),
            Arc::new(MockDetector),
            true,
        )
    }

    #[tokio::test]
    async fn a_disabled_manager_declines_a_session() {
        let mgr = OffloadSessionManager::new(
            Arc::new(DetectionBroadcaster::new(64)),
            Arc::new(MockDetector),
            false, // serving disabled
        );
        mgr.start_with_stream(spec("s-off"), VecFrameStream::new(vec![]))
            .await;
        assert_eq!(
            mgr.active_count().await,
            0,
            "a disabled node starts no session"
        );
    }

    fn spec(id: &str) -> SessionSpec {
        SessionSpec {
            id: id.into(),
            rtsp_url: "rtsp://drone.local:8554/main".into(),
            camera_id: "front".into(),
            width: 64,
            height: 48,
        }
    }

    #[test]
    fn from_job_params_reads_a_session_block() {
        let params = serde_json::json!({
            "session": { "id": "s1", "rtsp_url": "rtsp://d:8554/main", "camera_id": "front", "width": 640, "height": 480 }
        });
        let s = SessionSpec::from_job_params(&params).unwrap();
        assert_eq!(s.id, "s1");
        assert_eq!(s.rtsp_url, "rtsp://d:8554/main");
        assert_eq!(s.camera_id, "front");
        assert_eq!(s.width, 640);
        assert_eq!(s.height, 480);
    }

    #[test]
    fn from_job_params_defaults_the_frame_size_when_omitted() {
        let params = serde_json::json!({
            "session": { "id": "s1", "rtsp_url": "rtsp://d:8554/main", "camera_id": "front" }
        });
        let s = SessionSpec::from_job_params(&params).unwrap();
        assert_eq!(s.width, DEFAULT_WIDTH);
        assert_eq!(s.height, DEFAULT_HEIGHT);
    }

    #[test]
    fn a_per_frame_job_carries_no_session() {
        // The one-shot per-frame offload job (a `frame`, no `session`) yields None
        // so the worker takes the unchanged per-frame path.
        let params = serde_json::json!({ "frame": { "camera_id": "front", "width": 640, "height": 480, "ts_ms": 1 } });
        assert!(SessionSpec::from_job_params(&params).is_none());
        // A session block missing required fields is treated as absent, not fatal.
        let bad = serde_json::json!({ "session": { "camera_id": "front" } });
        assert!(SessionSpec::from_job_params(&bad).is_none());
        let empty_id = serde_json::json!({ "session": { "id": "", "rtsp_url": "rtsp://d/main", "camera_id": "front" } });
        assert!(SessionSpec::from_job_params(&empty_id).is_none());
    }

    #[test]
    fn work_priority_prefers_streaming_over_batch() {
        assert!(
            OffloadSessionManager::STREAMING_PRIORITY > OffloadSessionManager::BATCH_PRIORITY,
            "streaming outranks batch"
        );
    }

    #[tokio::test]
    async fn should_yield_to_streaming_only_when_a_session_is_live() {
        let mgr = manager();
        assert!(
            !mgr.should_yield_to_streaming(),
            "no session live -> no yield"
        );
        mgr.start_with_stream(
            spec("s1"),
            VecFrameStream::solid("front", 16, 16, 100_000, 0),
        )
        .await;
        assert!(
            mgr.should_yield_to_streaming(),
            "a live session -> batch yields to it"
        );
        assert!(mgr.stop("s1").await);
    }

    #[tokio::test]
    async fn start_with_stream_runs_a_session_and_reaps_it_on_end() {
        let mgr = manager();
        let mut rx = mgr.broadcaster().subscribe();
        // A finite synthetic stream: the session runs, emits, then ends.
        let stream = VecFrameStream::solid("front", 64, 48, 3, 1000);
        mgr.start_with_stream(spec("s1"), stream).await;

        // Three batches fan out over the broadcaster.
        for i in 0..3 {
            let b = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("a batch within 5s")
                .expect("broadcaster open");
            assert_eq!(b.session_id, "s1");
            assert_eq!(b.seq, i);
        }

        // The stream exhausted; the session self-reaps from the registry.
        for _ in 0..200 {
            if mgr.active_count().await == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(mgr.active_count().await, 0, "the ended session was reaped");
    }

    #[tokio::test]
    async fn a_resubmit_of_a_live_session_is_ignored() {
        let mgr = manager();
        // A long stream so the first session stays live across the second submit.
        mgr.start_with_stream(
            spec("s1"),
            VecFrameStream::solid("front", 16, 16, 100_000, 0),
        )
        .await;
        // It registers as live synchronously on start.
        assert_eq!(mgr.active_count().await, 1);
        // A re-submit of the same id must not start a second session.
        mgr.start_with_stream(
            spec("s1"),
            VecFrameStream::solid("front", 16, 16, 100_000, 0),
        )
        .await;
        assert_eq!(mgr.active_count().await, 1, "re-submit deduped");
        // Stop it cleanly.
        assert!(mgr.stop("s1").await);
    }

    #[tokio::test]
    async fn a_session_restarts_under_the_same_id_after_the_previous_one_ended() {
        let mgr = manager();
        let mut rx = mgr.broadcaster().subscribe();

        // First session over a finite stream: it runs, emits, then ends + self-reaps.
        mgr.start_with_stream(spec("s1"), VecFrameStream::solid("front", 64, 48, 2, 1000))
            .await;
        for i in 0..2 {
            let b = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("a batch within 5s")
                .expect("broadcaster open");
            assert_eq!(b.seq, i);
        }
        for _ in 0..200 {
            if mgr.active_count().await == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(mgr.active_count().await, 0, "the first session was reaped");

        // Re-open the SAME id: it must start a FRESH session (not be deduped
        // against the ended one), so batches flow again with seq restarting at 0.
        mgr.start_with_stream(spec("s1"), VecFrameStream::solid("front", 64, 48, 2, 2000))
            .await;
        let b = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("the re-opened session emits")
            .expect("broadcaster open");
        assert_eq!(b.session_id, "s1");
        assert_eq!(b.seq, 0, "a fresh session restarts the sequence");
    }

    #[tokio::test]
    async fn the_session_counter_tracks_live_sessions() {
        let mgr = manager();
        let counter = mgr.session_counter();
        assert_eq!(counter.load(Ordering::Relaxed), 0);

        mgr.start_with_stream(
            spec("s1"),
            VecFrameStream::solid("front", 16, 16, 100_000, 0),
        )
        .await;
        // The registration is synchronous with the start, so it is already 1 here.
        assert_eq!(counter.load(Ordering::Relaxed), 1);
        assert_eq!(
            mgr.active_count().await,
            1,
            "counter agrees with the registry"
        );

        assert!(mgr.stop("s1").await);
        // The decrement lands when the supervisor closes the session.
        for _ in 0..200 {
            if counter.load(Ordering::Relaxed) == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(
            counter.load(Ordering::Relaxed),
            0,
            "the counter decremented when the session ended"
        );
    }

    #[tokio::test]
    async fn stop_all_cancels_live_sessions() {
        let mgr = manager();
        mgr.start_with_stream(
            spec("a"),
            VecFrameStream::solid("front", 16, 16, 100_000, 0),
        )
        .await;
        mgr.start_with_stream(
            spec("b"),
            VecFrameStream::solid("front", 16, 16, 100_000, 0),
        )
        .await;
        assert_eq!(mgr.active_count().await, 2);
        mgr.stop_all().await;
        for _ in 0..200 {
            if mgr.active_count().await == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(
            mgr.active_count().await,
            0,
            "all sessions cancelled + reaped"
        );
    }

    /// A detector that always fails, counting its invocations, to drive the
    /// supervisor's backend-fault restart budget.
    struct FaultDetector {
        calls: Arc<AtomicU32>,
    }
    impl Detector for FaultDetector {
        fn name(&self) -> &str {
            "fault"
        }
        fn infer(
            &self,
            _frame: &FrameRef,
            _pixels: Option<&[u8]>,
        ) -> Result<Vec<Detection>, ComputeError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Err(ComputeError::Backend {
                backend: "fault".into(),
                message: "always fails".into(),
            })
        }
    }

    #[tokio::test]
    async fn a_backend_faulting_session_restarts_under_budget_then_gives_up() {
        let calls = Arc::new(AtomicU32::new(0));
        let mgr = OffloadSessionManager::new(
            Arc::new(DetectionBroadcaster::new(64)),
            Arc::new(FaultDetector {
                calls: calls.clone(),
            }),
            true,
        );
        // A factory that yields a FRESH one-frame stream each (re)start, so the
        // detector runs once per run and faults, exercising the restart budget.
        mgr.start_with_factory(spec("s-fault"), || {
            Some(VecFrameStream::solid("front", 8, 8, 1, 0))
        })
        .await;

        // The session gives up (never restart-loops forever) and closes.
        for _ in 0..600 {
            if mgr.active_count().await == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            mgr.active_count().await,
            0,
            "the persistently-faulting session gave up + closed"
        );
        // The detector ran once per run: 1 initial run + MAX_RESTARTS restarts.
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1 + OffloadSessionManager::MAX_RESTARTS,
            "restarts are bounded by the budget"
        );
    }
}
