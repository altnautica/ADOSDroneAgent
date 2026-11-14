//! Per-camera delivery rate for the status sidecar.
//!
//! A capture task counts its frames lock-free on a [`FrameClock`]; the
//! sidecar writer samples every clock about once a second and turns the
//! samples into a rate over the last [`RATE_WINDOW_NS`] ([`RateWindow`]).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use ados_protocol::vision_status::CameraRateStatus;

/// The window a camera's frame rate is measured over.
pub const RATE_WINDOW_NS: u64 = 2_000_000_000;

/// The frames one camera delivered, fed by its capture task.
#[derive(Debug, Default)]
pub(crate) struct FrameClock {
    frames: AtomicU64,
    /// Host monotonic arrival of the newest frame; 0 before the first.
    last_ns: AtomicU64,
}

impl FrameClock {
    /// A frame arrived at `now_ns` (host monotonic).
    pub(crate) fn frame_at(&self, now_ns: u64) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.last_ns.store(now_ns.max(1), Ordering::Relaxed);
    }
}

/// The writer's view of one camera: its clock and the frame counts sampled
/// from it, oldest first, trimmed to the window.
#[derive(Debug)]
pub(crate) struct RateWindow {
    clock: Arc<FrameClock>,
    samples: VecDeque<(u64, u64)>,
}

impl RateWindow {
    /// Start measuring `clock` from `now_ns`.
    pub(crate) fn new(clock: Arc<FrameClock>, now_ns: u64) -> Self {
        let frames = clock.frames.load(Ordering::Relaxed);
        Self {
            clock,
            samples: VecDeque::from([(now_ns, frames)]),
        }
    }

    /// The camera's rate and newest-frame age at `now_ns`: frames over the
    /// span back to the newest sample at least a window old (or the oldest
    /// there is). Both are null before the first frame.
    pub(crate) fn sample(&mut self, now_ns: u64) -> CameraRateStatus {
        let last = self.clock.last_ns.load(Ordering::Relaxed);
        let frames = self.clock.frames.load(Ordering::Relaxed);
        while self.samples.len() > 1 && now_ns.saturating_sub(self.samples[1].0) >= RATE_WINDOW_NS {
            self.samples.pop_front();
        }
        let (t0, f0) = self.samples.front().copied().unwrap_or((now_ns, frames));
        self.samples.push_back((now_ns, frames));
        if last == 0 {
            return CameraRateStatus::default();
        }
        let span = now_ns.saturating_sub(t0);
        let fps = (span > 0).then(|| {
            let hz = frames.saturating_sub(f0) as f64 * 1e9 / span as f64;
            (hz * 100.0).round() / 100.0
        });
        CameraRateStatus {
            fps,
            last_frame_age_ms: Some(now_ns.saturating_sub(last) / 1_000_000),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::work_status::{sidecar_path, write_status, WorkStatus};

    const MS: u64 = 1_000_000;

    #[tokio::test]
    async fn each_camera_reports_its_recent_rate_and_frame_age() {
        let status = WorkStatus::new();
        let mut main = status.input_at("main", 0);
        let _belly = status.input_at("belly", 0);
        // 10 frames in the first second.
        for i in 1..=10 {
            main.frame_at(i * 100 * MS);
        }
        let s = status.snapshot_at(1_000 * MS);
        let m = &s.cameras["main"];
        assert_eq!((m.fps, m.last_frame_age_ms), (Some(10.0), Some(0)));
        // A camera that has delivered nothing reads null, not zero.
        let b = &s.cameras["belly"];
        assert_eq!((b.fps, b.last_frame_age_ms), (None, None));

        // 30 more frames by 3 s (15 Hz), then silence: at 3.5 s the window
        // reaches back to the 1 s sample.
        for i in 1..=30 {
            main.frame_at(1_000 * MS + i * 2_000 * MS / 30);
        }
        let s = status.snapshot_at(3_500 * MS);
        let m = &s.cameras["main"];
        assert_eq!(m.fps, Some(12.0));
        assert_eq!(m.last_frame_age_ms, Some(500));

        // Silent long enough and the rate decays to zero; the age keeps growing.
        status.snapshot_at(5_000 * MS);
        let s = status.snapshot_at(7_000 * MS);
        let m = &s.cameras["main"];
        assert_eq!((m.fps, m.last_frame_age_ms), (Some(0.0), Some(4_000)));

        // The sidecar carries the map, nulls included.
        let dir = tempfile::tempdir().unwrap();
        let path = sidecar_path(&dir.path().to_string_lossy());
        write_status(&path, &s).await.unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            json["cameras"]["belly"],
            serde_json::json!({"fps": null, "last_frame_age_ms": null})
        );
        assert_eq!(json["cameras"]["main"]["last_frame_age_ms"], 4_000);
    }
}
