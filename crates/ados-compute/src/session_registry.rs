//! The node's streaming-session registry: a first-class, queryable record of
//! every live perception-offload session, its state machine, and the lock-free
//! active-session projection the heartbeat reads.
//!
//! A one-shot reconstruction is a queued job (see [`crate::scheduler`]); a
//! streaming perception-offload session is a different thing — a long-lived
//! open→flow→close feed that runs on its own task, outside the batch worker pool
//! (an NPU-less drone streams its camera to the node; the node detects and streams
//! detections back). This module holds one record per live session so the node can
//! report each session's state, throughput, and reconnect / restart history over
//! `/api/compute/sessions` — a real registry, not a bare count.
//!
//! The state machine is `Opening` (started, no batch yet) → `Live` (batches
//! flowing) → `Stalled` (no batch within the stall window while the frame reader
//! reconnects) → `Live` again on recovery, or `Closed` when the session ends. A
//! closed session is removed from the registry (the close reason is logged), so
//! the endpoint only ever lists live sessions. Every transition is driven by a
//! real signal — a batch emitted, a reader reconnect, the stall clock, a close —
//! never assumed (Rule 44).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

/// A streaming session with no detection batch for this long is considered
/// `Stalled`: still supervised (the frame reader is reconnecting), but not
/// currently producing, so a status surface reports the honest quiet state rather
/// than a stale `Live` (Rule 44).
pub const STALL_WINDOW_MS: i64 = 3_000;

/// Local epoch-ms clock for session timestamps.
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The lifecycle state of a streaming offload session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// Started; no detection batch has been emitted yet.
    Opening,
    /// Batches are flowing.
    Live,
    /// No batch within the stall window while the reader reconnects.
    Stalled,
    /// The session ended. A closed record is removed from the registry.
    Closed,
}

/// A session counts toward the heartbeat's `active_sessions` while it is coming up
/// or actively producing. A `Stalled` session (quiet / reconnecting) and a
/// `Closed` one do not.
fn counts_as_active(state: SessionState) -> bool {
    matches!(state, SessionState::Opening | SessionState::Live)
}

/// The priority of a unit of node work. Streaming perception offload is
/// latency-critical and outranks best-effort batch reconstruction, so the batch
/// worker yields to a live streaming session before starting its heavy backend.
/// Declared low→high so `Streaming > Batch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorkPriority {
    /// Best-effort reconstruction (splat / point cloud / mesh). Preemptible.
    Batch,
    /// A live perception-offload feed. Latency-critical; preempts batch.
    Streaming,
}

/// A streaming perception-offload session's live record, as served over the job
/// API. Snake-case fields to match the rest of the `/api/compute/*` surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamingSession {
    /// The session id (the WS path + the batch tag).
    pub id: String,
    /// The camera the detections are attributed to.
    pub camera_id: String,
    /// The source the node pulls (the drone's RTSP feed / device).
    pub source: String,
    pub state: SessionState,
    pub started_at_ms: i64,
    /// Frames the detector ran on (== `batches_emitted`; one batch per processed
    /// frame).
    pub frames_processed: u64,
    pub batches_emitted: u64,
    /// Epoch-ms of the last emitted batch; `None` until the first batch.
    pub last_batch_at_ms: Option<i64>,
    /// Frame-reader reconnect retries (RTSP / ffmpeg hiccups) this session.
    pub reconnects: u32,
    /// Task-level restarts by the supervisor (a backend fault / a panic).
    pub restarts: u32,
}

/// A session record plus its snapshot-time uptime, as returned by
/// `GET /api/compute/sessions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionView {
    #[serde(flatten)]
    pub session: StreamingSession,
    /// Milliseconds since the session opened, at snapshot time.
    pub uptime_ms: i64,
}

/// The per-state count breakdown folded onto `GET /api/compute/status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionStateCounts {
    pub opening: u32,
    pub live: u32,
    pub stalled: u32,
}

/// One registry entry: the public record plus the cancel handle the manager
/// stops it with (kept out of the serialized record).
struct SessionEntry {
    session: StreamingSession,
    cancel: Arc<Notify>,
}

/// The live streaming-session registry. The single source of truth for every
/// session's state; the manager mutates it and the job API reads it. Holds a
/// lock-free `Opening|Live` projection so the engine's synchronous heartbeat reads
/// `active_sessions` without locking.
pub struct SessionRegistry {
    inner: Mutex<HashMap<String, SessionEntry>>,
    /// Lock-free projection = count of `Opening|Live` records, shared into the
    /// engine so its sync heartbeat reports `active_sessions` with no lock.
    /// Recomputed from the map on every mutation, so it can never drift from the
    /// records it summarizes (Rule 44).
    active: Arc<AtomicU32>,
}

impl SessionRegistry {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            active: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Lock the map, recovering the guard if a prior holder panicked (the critical
    /// sections here never panic mid-mutation, so the data stays consistent).
    fn lock(&self) -> MutexGuard<'_, HashMap<String, SessionEntry>> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Recompute the `Opening|Live` projection from the map under a held lock.
    fn refresh_active(map: &HashMap<String, SessionEntry>, active: &AtomicU32) {
        let n = map
            .values()
            .filter(|e| counts_as_active(e.session.state))
            .count();
        active.store(n as u32, Ordering::Relaxed);
    }

    /// The shared `active_sessions` projection (Opening|Live count) for the
    /// engine's heartbeat. The clone is handed to `Engine::set_session_counter`.
    pub(crate) fn active_counter(&self) -> Arc<AtomicU32> {
        self.active.clone()
    }

    /// Register a new session in `Opening` with its cancel handle. Returns `false`
    /// when a live record for `id` already exists (dedup — the caller declines the
    /// duplicate). A closed session is removed, so a re-open of a prior id sees no
    /// record and registers fresh.
    pub(crate) fn register(
        &self,
        id: &str,
        camera_id: &str,
        source: &str,
        cancel: Arc<Notify>,
        now_ms: i64,
    ) -> bool {
        let mut map = self.lock();
        if map.contains_key(id) {
            return false;
        }
        map.insert(
            id.to_string(),
            SessionEntry {
                session: StreamingSession {
                    id: id.to_string(),
                    camera_id: camera_id.to_string(),
                    source: source.to_string(),
                    state: SessionState::Opening,
                    started_at_ms: now_ms,
                    frames_processed: 0,
                    batches_emitted: 0,
                    last_batch_at_ms: None,
                    reconnects: 0,
                    restarts: 0,
                },
                cancel,
            },
        );
        Self::refresh_active(&map, &self.active);
        true
    }

    /// A batch was emitted: mark the session `Live`, bump its throughput counters,
    /// and stamp the emit time. A no-op if the session is gone (already closed).
    pub(crate) fn on_batch(&self, id: &str, now_ms: i64) {
        let mut map = self.lock();
        if let Some(e) = map.get_mut(id) {
            e.session.state = SessionState::Live;
            e.session.frames_processed = e.session.frames_processed.saturating_add(1);
            e.session.batches_emitted = e.session.batches_emitted.saturating_add(1);
            e.session.last_batch_at_ms = Some(now_ms);
        }
        Self::refresh_active(&map, &self.active);
    }

    /// The frame reader hit a transient error and is reconnecting: mark the
    /// session `Stalled` (unless it is already closed) and count the retry.
    pub(crate) fn on_reconnect(&self, id: &str) {
        let mut map = self.lock();
        if let Some(e) = map.get_mut(id) {
            if e.session.state != SessionState::Closed {
                e.session.state = SessionState::Stalled;
            }
            e.session.reconnects = e.session.reconnects.saturating_add(1);
        }
        Self::refresh_active(&map, &self.active);
    }

    /// The supervisor restarted the session task after a backend fault or a panic.
    pub(crate) fn note_restart(&self, id: &str) {
        let mut map = self.lock();
        if let Some(e) = map.get_mut(id) {
            e.session.restarts = e.session.restarts.saturating_add(1);
        }
    }

    /// Flip any `Live` session with no batch within `window_ms` to `Stalled`, so a
    /// session whose source went quiet without a reader error (the drone paused,
    /// the feed froze) reads honestly rather than as a stale `Live`. Returns the
    /// stalled ids. The stall watchdog calls this on a tick.
    pub(crate) fn stall_quiet_sessions(&self, now_ms: i64, window_ms: i64) -> Vec<String> {
        let mut map = self.lock();
        let mut stalled = Vec::new();
        for (id, e) in map.iter_mut() {
            if e.session.state == SessionState::Live {
                let last = e
                    .session
                    .last_batch_at_ms
                    .unwrap_or(e.session.started_at_ms);
                if now_ms - last > window_ms {
                    e.session.state = SessionState::Stalled;
                    stalled.push(id.clone());
                }
            }
        }
        Self::refresh_active(&map, &self.active);
        stalled
    }

    /// Close (and remove) the session. Returns whether a record existed.
    pub(crate) fn close(&self, id: &str) -> bool {
        let mut map = self.lock();
        let existed = map.remove(id).is_some();
        Self::refresh_active(&map, &self.active);
        existed
    }

    /// The cancel handle for a live session, to stop it. `None` when no live
    /// session has that id.
    pub(crate) fn cancel_of(&self, id: &str) -> Option<Arc<Notify>> {
        self.lock().get(id).map(|e| e.cancel.clone())
    }

    /// Every live session's cancel handle (for shutdown).
    pub(crate) fn all_cancels(&self) -> Vec<Arc<Notify>> {
        self.lock().values().map(|e| e.cancel.clone()).collect()
    }

    /// Total live records (Opening|Live|Stalled). A session is "alive" while it
    /// holds a record.
    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether any session is live (the batch-yields-to-streaming signal).
    pub(crate) fn any_live(&self) -> bool {
        !self.lock().is_empty()
    }

    /// Snapshot every live session as a serializable view, oldest first.
    pub(crate) fn snapshot(&self, now_ms: i64) -> Vec<SessionView> {
        let map = self.lock();
        let mut out: Vec<SessionView> = map
            .values()
            .map(|e| SessionView {
                session: e.session.clone(),
                uptime_ms: (now_ms - e.session.started_at_ms).max(0),
            })
            .collect();
        out.sort_by_key(|v| v.session.started_at_ms);
        out
    }

    /// The per-state count breakdown (for the status enrichment).
    pub(crate) fn state_counts(&self) -> SessionStateCounts {
        let map = self.lock();
        let mut c = SessionStateCounts::default();
        for e in map.values() {
            match e.session.state {
                SessionState::Opening => c.opening += 1,
                SessionState::Live => c.live += 1,
                SessionState::Stalled => c.stalled += 1,
                SessionState::Closed => {}
            }
        }
        c
    }
}

/// A handle the runner uses to report streaming-session progress to the registry
/// as it runs. Cheap to clone (it moves into the frame-reader task); all methods
/// are best-effort no-ops when detached (tests + callers with no registry).
#[derive(Clone)]
pub struct SessionProgress {
    registry: Option<Arc<SessionRegistry>>,
    id: String,
}

impl SessionProgress {
    /// A progress handle bound to `registry` for session `id`.
    pub(crate) fn bound(registry: Arc<SessionRegistry>, id: String) -> Self {
        Self {
            registry: Some(registry),
            id,
        }
    }

    /// A no-op progress handle (tests + non-registry callers of the runner).
    pub fn detached() -> Self {
        Self {
            registry: None,
            id: String::new(),
        }
    }

    /// Report that a detection batch was emitted (→ `Live` + throughput counters).
    pub fn on_batch(&self) {
        if let Some(r) = &self.registry {
            r.on_batch(&self.id, now_ms());
        }
    }

    /// Report that the frame reader is reconnecting after a hiccup (→ `Stalled`).
    pub fn on_reconnect(&self) {
        if let Some(r) = &self.registry {
            r.on_reconnect(&self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cancel() -> Arc<Notify> {
        Arc::new(Notify::new())
    }

    #[test]
    fn a_registered_session_opens_and_counts_as_active() {
        let r = SessionRegistry::new();
        assert!(r.register("s1", "front", "rtsp://d/main", cancel(), 1000));
        assert_eq!(r.len(), 1);
        // Opening counts toward the active projection.
        assert_eq!(r.active_counter().load(Ordering::Relaxed), 1);
        let s = &r.snapshot(1000)[0].session;
        assert_eq!(s.state, SessionState::Opening);
        assert_eq!(s.frames_processed, 0);
        assert_eq!(s.last_batch_at_ms, None);
    }

    #[test]
    fn a_duplicate_register_is_declined() {
        let r = SessionRegistry::new();
        assert!(r.register("s1", "front", "rtsp://d/main", cancel(), 1000));
        assert!(
            !r.register("s1", "front", "rtsp://d/main", cancel(), 1000),
            "a live id is deduped"
        );
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn the_state_machine_walks_opening_live_stalled_live_closed() {
        let r = SessionRegistry::new();
        let active = r.active_counter();
        r.register("s1", "front", "rtsp://d/main", cancel(), 1000);
        assert_eq!(state(&r, "s1"), SessionState::Opening);
        assert_eq!(active.load(Ordering::Relaxed), 1);

        // A batch → Live + counters + last-batch stamp.
        r.on_batch("s1", 1100);
        let s = session(&r, "s1");
        assert_eq!(s.state, SessionState::Live);
        assert_eq!(s.frames_processed, 1);
        assert_eq!(s.batches_emitted, 1);
        assert_eq!(s.last_batch_at_ms, Some(1100));
        assert_eq!(active.load(Ordering::Relaxed), 1, "Live still counts");

        // Quiet past the window → Stalled, and it drops OUT of the active count.
        let stalled = r.stall_quiet_sessions(1100 + STALL_WINDOW_MS + 1, STALL_WINDOW_MS);
        assert_eq!(stalled, vec!["s1".to_string()]);
        assert_eq!(state(&r, "s1"), SessionState::Stalled);
        assert_eq!(active.load(Ordering::Relaxed), 0, "Stalled is not active");

        // Batches resume → Live, back in the active count.
        r.on_batch("s1", 5000);
        assert_eq!(state(&r, "s1"), SessionState::Live);
        assert_eq!(active.load(Ordering::Relaxed), 1);

        // Close removes it.
        assert!(r.close("s1"));
        assert_eq!(r.len(), 0);
        assert_eq!(active.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_reconnect_stalls_and_counts_the_retry() {
        let r = SessionRegistry::new();
        r.register("s1", "front", "rtsp://d/main", cancel(), 1000);
        r.on_batch("s1", 1100); // Live
        r.on_reconnect("s1");
        let s = session(&r, "s1");
        assert_eq!(s.state, SessionState::Stalled);
        assert_eq!(s.reconnects, 1);
        assert_eq!(
            r.active_counter().load(Ordering::Relaxed),
            0,
            "a reconnecting session is not counted active"
        );
    }

    #[test]
    fn note_restart_bumps_the_restart_count() {
        let r = SessionRegistry::new();
        r.register("s1", "front", "rtsp://d/main", cancel(), 1000);
        r.note_restart("s1");
        r.note_restart("s1");
        assert_eq!(session(&r, "s1").restarts, 2);
    }

    #[test]
    fn stall_only_touches_live_sessions_that_went_quiet() {
        let r = SessionRegistry::new();
        // Opening (no batch yet) is not stalled by the watchdog — only Live goes quiet.
        r.register("opening", "c", "rtsp://d/1", cancel(), 1000);
        r.register("live", "c", "rtsp://d/2", cancel(), 1000);
        r.on_batch("live", 1000);
        // A very recent batch: not stalled.
        let stalled = r.stall_quiet_sessions(1000 + STALL_WINDOW_MS, STALL_WINDOW_MS);
        assert!(stalled.is_empty(), "within the window stays Live");
        assert_eq!(state(&r, "live"), SessionState::Live);
        // Past the window: only the Live one stalls.
        let stalled = r.stall_quiet_sessions(1000 + STALL_WINDOW_MS + 1, STALL_WINDOW_MS);
        assert_eq!(stalled, vec!["live".to_string()]);
        assert_eq!(state(&r, "opening"), SessionState::Opening);
    }

    #[test]
    fn state_counts_break_down_by_state() {
        let r = SessionRegistry::new();
        r.register("a", "c", "rtsp://d/1", cancel(), 1000); // Opening
        r.register("b", "c", "rtsp://d/2", cancel(), 1000);
        r.on_batch("b", 1000); // Live
        r.register("c", "c", "rtsp://d/3", cancel(), 1000);
        r.on_batch("c", 1000);
        r.on_reconnect("c"); // Stalled
        let counts = r.state_counts();
        assert_eq!(counts.opening, 1);
        assert_eq!(counts.live, 1);
        assert_eq!(counts.stalled, 1);
    }

    #[test]
    fn snapshot_serializes_the_session_fields_with_uptime() {
        let r = SessionRegistry::new();
        r.register("s1", "front", "rtsp://d/main", cancel(), 1000);
        r.on_batch("s1", 1500);
        let views = r.snapshot(2000);
        let v = serde_json::to_value(&views[0]).unwrap();
        assert_eq!(v["id"], "s1");
        assert_eq!(v["camera_id"], "front");
        assert_eq!(v["source"], "rtsp://d/main");
        assert_eq!(v["state"], "live");
        assert_eq!(v["frames_processed"], 1);
        assert_eq!(v["batches_emitted"], 1);
        assert_eq!(v["last_batch_at_ms"], 1500);
        assert_eq!(v["reconnects"], 0);
        assert_eq!(v["restarts"], 0);
        assert_eq!(v["uptime_ms"], 1000); // 2000 - 1000
    }

    #[test]
    fn work_priority_orders_streaming_above_batch() {
        assert!(WorkPriority::Streaming > WorkPriority::Batch);
    }

    #[test]
    fn a_detached_progress_handle_is_a_no_op() {
        // A detached handle touches no registry and never panics.
        let p = SessionProgress::detached();
        p.on_batch();
        p.on_reconnect();
    }

    fn session(r: &SessionRegistry, id: &str) -> StreamingSession {
        r.snapshot(0)
            .into_iter()
            .find(|v| v.session.id == id)
            .expect("session present")
            .session
    }

    fn state(r: &SessionRegistry, id: &str) -> SessionState {
        session(r, id).state
    }
}
