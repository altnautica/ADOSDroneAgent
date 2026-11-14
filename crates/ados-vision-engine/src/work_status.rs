//! The engine's work counters and the sidecar that publishes them.
//!
//! The supervisor cannot see this engine work in its process I/O counters,
//! because every frame arrives over a Unix socket (see
//! [`ados_protocol::vision_status`]). So each capture task counts the frames
//! it consumes and marks its input live while that input delivers, and
//! [`run_writer`] publishes both about once a second, with each camera's
//! recent frame rate and newest-frame age.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ados_protocol::framebus::mono_ns;
use ados_protocol::vision_status::{
    SourceError, VisionStatus, SOURCE_SILENT_PREFIX, VISION_STATUS_FILE, VISION_STATUS_VERSION,
};

use crate::work_rate::{FrameClock, RateWindow};

/// How often the sidecar is rewritten.
pub const WRITE_INTERVAL: Duration = Duration::from_secs(1);

/// How long an open input may deliver nothing before its capture task closes
/// and reopens it. Far longer than the gap between frames of the slowest tap,
/// and well inside the supervisor's stall window, so a silent upstream drops
/// the input out of the live set long before it could read as a stalled engine.
pub const INPUT_SILENCE_TIMEOUT: Duration = Duration::from_secs(10);

/// The shared counters every capture task feeds.
#[derive(Debug, Default)]
pub struct WorkStatus {
    frames_consumed: AtomicU64,
    live_inputs: AtomicU32,
    error_frames: AtomicU64,
    source_errors: Mutex<BTreeMap<String, String>>,
    cameras: Mutex<BTreeMap<String, RateWindow>>,
}

impl WorkStatus {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The current state, as the sidecar carries it.
    pub fn snapshot(&self) -> VisionStatus {
        self.snapshot_at(mono_ns())
    }

    /// The state at `now_ns` (host monotonic); the camera rates are measured
    /// up to it.
    pub fn snapshot_at(&self, now_ns: u64) -> VisionStatus {
        let cameras = self
            .cameras
            .lock()
            .expect("camera rates lock")
            .iter_mut()
            .map(|(id, window)| (id.clone(), window.sample(now_ns)))
            .collect();
        let errors = self.source_errors.lock().expect("source errors lock");
        VisionStatus {
            version: VISION_STATUS_VERSION,
            frames_consumed: self.frames_consumed.load(Ordering::Relaxed),
            live_inputs: self.live_inputs.load(Ordering::Relaxed),
            error_frames: self.error_frames.load(Ordering::Relaxed),
            source_errors: errors
                .iter()
                .map(|(camera, error)| SourceError {
                    camera: camera.clone(),
                    error: error.clone(),
                })
                .collect(),
            detection: None,
            depth: None,
            cameras,
        }
    }

    /// `n` more corrupt buffers were dropped.
    pub fn add_error_frames(&self, n: u64) {
        self.error_frames.fetch_add(n, Ordering::Relaxed);
    }

    /// Record (or, with `None`, clear) a camera's source failure.
    pub fn set_source_error(&self, camera: &str, error: Option<String>) {
        let mut errors = self.source_errors.lock().expect("source errors lock");
        match error {
            Some(e) => {
                errors.insert(camera.to_string(), e);
            }
            None => {
                errors.remove(camera);
            }
        }
    }

    /// A handle `camera`'s capture task holds for its input. The camera is
    /// listed in the sidecar's rates from now on, null until its first frame.
    pub fn input(self: &Arc<Self>, camera: &str) -> InputLiveness {
        self.input_at(camera, mono_ns())
    }

    /// [`input`](Self::input), measuring the camera's rate from `now_ns`.
    pub fn input_at(self: &Arc<Self>, camera: &str, now_ns: u64) -> InputLiveness {
        let clock = Arc::new(FrameClock::default());
        self.cameras
            .lock()
            .expect("camera rates lock")
            .insert(camera.to_string(), RateWindow::new(clock.clone(), now_ns));
        InputLiveness {
            status: self.clone(),
            clock,
            live: false,
        }
    }
}

/// One input's contribution to the live set. Live from its first delivered
/// frame until [`lost`](Self::lost) or drop.
#[derive(Debug)]
pub struct InputLiveness {
    status: Arc<WorkStatus>,
    clock: Arc<FrameClock>,
    live: bool,
}

impl InputLiveness {
    /// A frame was consumed from this input.
    pub fn frame(&mut self) {
        self.frame_at(mono_ns());
    }

    /// A frame arrived from this input at `now_ns` (host monotonic).
    pub fn frame_at(&mut self, now_ns: u64) {
        self.clock.frame_at(now_ns);
        self.status.frames_consumed.fetch_add(1, Ordering::Relaxed);
        if !self.live {
            self.live = true;
            self.status.live_inputs.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The input failed, ended or went silent; it no longer counts as live.
    pub fn lost(&mut self) {
        if self.live {
            self.live = false;
            self.status.live_inputs.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl Drop for InputLiveness {
    fn drop(&mut self) {
        self.lost();
    }
}

/// The source-error reason for an input that delivered nothing for `silence`.
pub fn silence_reason(silence: Duration) -> String {
    format!("{SOURCE_SILENT_PREFIX}{} s", silence.as_secs())
}

/// Note a camera's source failure: record `reason` in the sidecar unless it is
/// already the camera's `last_error`. Returns `true` when the reason is new, so
/// the caller logs it at warn once per distinct reason; the next delivered
/// frame clears `last_error` and the sidecar entry.
pub fn note_source_failure(
    work: &WorkStatus,
    last_error: &mut Option<String>,
    camera: &str,
    reason: String,
) -> bool {
    if last_error.as_deref() == Some(reason.as_str()) {
        return false;
    }
    work.set_source_error(camera, Some(reason.clone()));
    *last_error = Some(reason);
    true
}

/// A camera's source delivered a frame: clear any noted failure.
pub fn note_source_recovered(work: &WorkStatus, last_error: &mut Option<String>, camera: &str) {
    if last_error.take().is_some() {
        work.set_source_error(camera, None);
    }
}

/// The sidecar path under the engine's socket directory.
pub fn sidecar_path(socket_dir: &str) -> PathBuf {
    Path::new(socket_dir).join(VISION_STATUS_FILE)
}

/// Replace the sidecar with `status` through a temp file and a rename, so a
/// reader never sees a torn file.
pub async fn write_status(path: &Path, status: &VisionStatus) -> std::io::Result<()> {
    let body = serde_json::to_vec(status).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, body).await?;
    tokio::fs::rename(&tmp, path).await
}

/// Publish `status` to `path` every [`WRITE_INTERVAL`] until `cancel` fires,
/// with `fill` adding the live demand blocks (the detector's and the depth
/// layer's) on each write. A failed write is logged once per failure streak
/// and retried on the next tick; the engine keeps running either way.
pub async fn run_writer(
    status: Arc<WorkStatus>,
    fill: impl Fn(&mut VisionStatus),
    path: PathBuf,
    cancel: ados_protocol::shutdown::Shutdown,
) {
    let mut tick = tokio::time::interval(WRITE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut failing = false;
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = cancel.wait() => return,
        }
        let mut snapshot = status.snapshot();
        fill(&mut snapshot);
        match write_status(&path, &snapshot).await {
            Ok(()) => failing = false,
            Err(e) => {
                if !failing {
                    tracing::warn!(path = %path.display(), error = %e, "vision_status_write_failed");
                }
                failing = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repeated_silence_is_noted_once_and_cleared_by_a_frame() {
        let status = WorkStatus::new();
        let mut last = None;
        let reason = silence_reason(INPUT_SILENCE_TIMEOUT);
        assert_eq!(reason, "delivered no frame for 10 s");
        assert!(note_source_failure(
            &status,
            &mut last,
            "cam",
            reason.clone()
        ));
        assert!(!note_source_failure(
            &status,
            &mut last,
            "cam",
            reason.clone()
        ));
        let s = status.snapshot();
        assert_eq!(s.source_errors.len(), 1);
        assert_eq!(s.source_errors[0].error, reason);

        note_source_recovered(&status, &mut last, "cam");
        assert!(last.is_none());
        assert!(status.snapshot().source_errors.is_empty());
        // Silent again after recovering: noted afresh.
        assert!(note_source_failure(&status, &mut last, "cam", reason));
    }

    #[test]
    fn a_different_failure_reason_is_noted_again() {
        let status = WorkStatus::new();
        let mut last = None;
        assert!(note_source_failure(&status, &mut last, "cam", "eof".into()));
        let silence = silence_reason(INPUT_SILENCE_TIMEOUT);
        assert!(note_source_failure(
            &status,
            &mut last,
            "cam",
            silence.clone()
        ));
        assert_eq!(status.snapshot().source_errors[0].error, silence);
        // Consumers tell a silent source from a failed one by the reason.
        assert!(status.snapshot().source_errors[0].is_silence());
        assert!(!SourceError {
            camera: "cam".into(),
            error: "eof".into()
        }
        .is_silence());
    }

    #[test]
    fn source_errors_and_corrupt_frames_reach_the_snapshot() {
        let status = WorkStatus::new();
        status.add_error_frames(3);
        status.set_source_error("cam_fr", Some("invalid cameras[1] entry".into()));
        let s = status.snapshot();
        assert_eq!(s.error_frames, 3);
        assert_eq!(s.source_errors.len(), 1);
        assert_eq!(s.source_errors[0].camera, "cam_fr");
        status.set_source_error("cam_fr", None);
        assert!(status.snapshot().source_errors.is_empty());
    }

    #[test]
    fn an_input_is_live_from_its_first_frame_until_it_is_lost_or_dropped() {
        let status = WorkStatus::new();
        let mut a = status.input("a");
        let mut b = status.input("b");
        // Opened but not yet delivering: not live, nothing to judge.
        assert_eq!(status.snapshot().work_counter(), None);

        a.frame();
        a.frame();
        b.frame();
        let s = status.snapshot();
        assert_eq!((s.frames_consumed, s.live_inputs), (3, 2));
        assert_eq!(s.work_counter(), Some(3));

        // A silent or failed input leaves the live set; the count stays.
        a.lost();
        a.lost();
        assert_eq!(status.snapshot().live_inputs, 1);
        drop(b);
        let s = status.snapshot();
        assert_eq!((s.frames_consumed, s.live_inputs), (3, 0));
        assert_eq!(s.work_counter(), None);

        // A reopened input that delivers again is live again.
        a.frame();
        assert_eq!(status.snapshot().work_counter(), Some(4));
    }

    #[tokio::test]
    async fn the_sidecar_carries_the_live_counters() {
        let dir = tempfile::tempdir().unwrap();
        let path = sidecar_path(&dir.path().to_string_lossy());
        let status = WorkStatus::new();
        let mut input = status.input("main");
        input.frame();
        write_status(&path, &status.snapshot()).await.unwrap();
        let read: VisionStatus =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read.version, VISION_STATUS_VERSION);
        assert_eq!(read.work_counter(), Some(1));
    }
}
