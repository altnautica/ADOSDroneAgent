//! The engine's work counters and the sidecar that publishes them.
//!
//! The supervisor cannot see this engine work in its process I/O counters,
//! because every frame arrives over a Unix socket (see
//! [`ados_protocol::vision_status`]). So each capture task counts the frames
//! it consumes and marks its input live while that input delivers, and
//! [`run_writer`] publishes both about once a second.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::vision_status::{VisionStatus, VISION_STATUS_FILE, VISION_STATUS_VERSION};

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
}

impl WorkStatus {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The current state, as the sidecar carries it.
    pub fn snapshot(&self) -> VisionStatus {
        VisionStatus {
            version: VISION_STATUS_VERSION,
            frames_consumed: self.frames_consumed.load(Ordering::Relaxed),
            live_inputs: self.live_inputs.load(Ordering::Relaxed),
        }
    }

    /// A handle one capture task holds for its input.
    pub fn input(self: &Arc<Self>) -> InputLiveness {
        InputLiveness {
            status: self.clone(),
            live: false,
        }
    }
}

/// One input's contribution to the live set. Live from its first delivered
/// frame until [`lost`](Self::lost) or drop.
#[derive(Debug)]
pub struct InputLiveness {
    status: Arc<WorkStatus>,
    live: bool,
}

impl InputLiveness {
    /// A frame was consumed from this input.
    pub fn frame(&mut self) {
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

/// Publish `status` to `path` every [`WRITE_INTERVAL`] until `cancel` fires.
/// A failed write is logged once per failure streak and retried on the next
/// tick; the engine keeps running either way.
pub async fn run_writer(
    status: Arc<WorkStatus>,
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
        match write_status(&path, &status.snapshot()).await {
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
    fn an_input_is_live_from_its_first_frame_until_it_is_lost_or_dropped() {
        let status = WorkStatus::new();
        let mut a = status.input();
        let mut b = status.input();
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
        let mut input = status.input();
        input.frame();
        write_status(&path, &status.snapshot()).await.unwrap();
        let read: VisionStatus =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read.version, VISION_STATUS_VERSION);
        assert_eq!(read.work_counter(), Some(1));
    }
}
