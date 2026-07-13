//! Node-side streaming-session manager: the auto-start half of the offload path.
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
//! spec here — [`OffloadSessionManager::start`] spawns [`run_offload_session`]
//! over an [`RtspFrameStream`] pulling the drone's feed and pumps the emitted
//! batches into the shared [`DetectionBroadcaster`] the WS router fans out. The
//! one-shot per-frame path is untouched; a session job simply takes this branch.
//!
//! Lifecycle: sessions are keyed by id, so a re-submit of a live session is a
//! no-op (idempotent), and a session self-removes from the active set when its
//! frame stream ends (the drone re-opens it). The daemon holds one manager for
//! its lifetime; [`OffloadSessionManager::stop_all`] tears every session down on
//! shutdown.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use serde::Deserialize;
use tokio::sync::{mpsc, Mutex, Notify};

use crate::offload::Detector;
use crate::offload_stream::{run_offload_session, OffloadFrameStream, RtspFrameStream};
use crate::offload_ws::{pump_to_broadcaster, DetectionBroadcaster};

/// The default RGB24 frame size assumed when a session spec omits it. The drone
/// normally advertises its camera size in the spec; this is the safety default
/// so a spec missing it still names a concrete fixed-frame size for the decoder.
const DEFAULT_WIDTH: u32 = 1280;
const DEFAULT_HEIGHT: u32 = 720;
/// Per-session channel + broadcaster buffer depth (batches). A slow WS subscriber
/// past the buffer lags and skips (never blocking the detector); the pump keeps
/// pace with the detector on this bound.
const SESSION_CHANNEL_CAP: usize = 64;

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

/// Owns the node's active streaming sessions + the shared detection broadcaster
/// the WS router fans out. Held by the daemon for its lifetime.
pub struct OffloadSessionManager {
    broadcaster: Arc<DetectionBroadcaster>,
    detector: Arc<dyn Detector>,
    /// Whether the node serves offload sessions at all (the
    /// `perception.serving.enabled` toggle). When false, `start` declines so the
    /// node reconstructs but never runs perception offload for a drone.
    serve_offload: bool,
    /// session id -> its cancel handle. `Arc<Mutex<..>>` so a session's own
    /// self-cleanup task can remove its entry when its stream ends.
    active: Arc<Mutex<HashMap<String, Arc<Notify>>>>,
    /// Live session count, shared with the engine's heartbeat so a node streaming
    /// to N drones reports N active offload sessions (not 0). Incremented when a
    /// session starts and decremented when it ends, in lockstep with `active`.
    session_counter: Arc<AtomicU32>,
}

impl OffloadSessionManager {
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
            active: Arc::new(Mutex::new(HashMap::new())),
            session_counter: Arc::new(AtomicU32::new(0)),
        }
    }

    /// The live-session counter, shared into the engine so its heartbeat reflects
    /// active offload sessions.
    pub fn session_counter(&self) -> Arc<AtomicU32> {
        self.session_counter.clone()
    }

    /// Start (or re-affirm) the session described by `spec`, pulling the drone's
    /// RTSP feed. Idempotent: a spec whose id is already live is a no-op, so a
    /// re-submit (a lossy-link retry) never doubles the session.
    pub async fn start(&self, spec: SessionSpec) {
        let stream = RtspFrameStream::new(
            spec.camera_id.clone(),
            spec.rtsp_url.clone(),
            spec.width,
            spec.height,
        );
        self.start_with_stream(spec, stream).await;
    }

    /// Start a session over an injected frame stream. `start` calls this with a
    /// live [`RtspFrameStream`]; tests call it with a synthetic
    /// [`crate::VecFrameStream`] so the manager's lifecycle is exercised with no
    /// ffmpeg / camera / RTSP.
    pub async fn start_with_stream<S>(&self, spec: SessionSpec, stream: S)
    where
        S: OffloadFrameStream + 'static,
    {
        if !self.serve_offload {
            tracing::info!(session = %spec.id, "offload serving disabled by config; session declined");
            return;
        }
        let cancel = {
            let mut active = self.active.lock().await;
            if active.contains_key(&spec.id) {
                tracing::info!(session = %spec.id, "offload session already running; re-submit ignored");
                return;
            }
            let cancel = Arc::new(Notify::new());
            active.insert(spec.id.clone(), cancel.clone());
            // In lockstep with `active`: a real new session bumps the live count.
            self.session_counter.fetch_add(1, Ordering::Relaxed);
            cancel
        };

        // The session emits one batch per frame onto `tx`; the pump forwards them
        // into the broadcaster the WS router fans out to the drone's subscriber.
        let (tx, rx) = mpsc::channel(SESSION_CHANNEL_CAP);
        tokio::spawn(pump_to_broadcaster(
            spec.id.clone(),
            rx,
            self.broadcaster.clone(),
        ));

        let detector = self.detector.clone();
        let active = self.active.clone();
        let counter = self.session_counter.clone();
        let id = spec.id.clone();
        let camera_id = spec.camera_id.clone();
        tokio::spawn(async move {
            run_offload_session(&id, &camera_id, stream, detector, tx, cancel).await;
            // The stream ended (or was cancelled); drop the entry so a later
            // re-open of the same id starts a fresh session rather than being
            // deduped against a dead one.
            active.lock().await.remove(&id);
            counter.fetch_sub(1, Ordering::Relaxed);
            tracing::info!(session = %id, "offload session ended");
        });
        tracing::info!(session = %spec.id, camera = %spec.camera_id, "offload session started");
    }

    /// Stop the named session (best-effort). Returns whether it was live. The
    /// session's own task removes it from the active set as it unwinds.
    pub async fn stop(&self, session_id: &str) -> bool {
        let cancel = self.active.lock().await.get(session_id).cloned();
        match cancel {
            Some(c) => {
                c.notify_one();
                true
            }
            None => false,
        }
    }

    /// Cancel every live session (daemon shutdown). Each session's task reaps its
    /// own entry as it stops.
    pub async fn stop_all(&self) {
        let active = self.active.lock().await;
        for cancel in active.values() {
            cancel.notify_one();
        }
    }

    /// How many sessions are currently live (test/introspection).
    pub async fn active_count(&self) -> usize {
        self.active.lock().await.len()
    }

    /// The shared broadcaster (the WS router mounts on it).
    pub fn broadcaster(&self) -> Arc<DetectionBroadcaster> {
        self.broadcaster.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::offload_ws::DetectionBroadcaster;
    use crate::{MockDetector, VecFrameStream};

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

        // The stream exhausted; the session self-reaps from the active set.
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
        // Wait until it registers as live.
        for _ in 0..200 {
            if mgr.active_count().await == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
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
        // The increment is synchronous with the start, so it is already 1 here.
        assert_eq!(counter.load(Ordering::Relaxed), 1);
        assert_eq!(
            mgr.active_count().await,
            1,
            "counter agrees with the active map"
        );

        assert!(mgr.stop("s1").await);
        // The decrement lands when the session task unwinds.
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
        for _ in 0..200 {
            if mgr.active_count().await == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
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
}
