//! The engine side of the depth layer: the depth model, its runs on the shared
//! accelerator, and the records it publishes.
//!
//! A depth run takes the accelerator only when no other inference is waiting
//! for it: the detector (and every other model) has priority, and a depth run
//! that cannot get the accelerator within [`DEPTH_PERMIT_WAIT`] skips that
//! frame. Runs are paced per camera by the caller.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use ados_protocol::framebus::FrameDescriptor;
use ados_protocol::vision_depth::DepthRecord;
use ados_protocol::vision_status::{CameraDepthStatus, DepthStatus};
use tokio::sync::SemaphorePermit;

use super::*;
use crate::depth::geometry::CameraGeometry;
use crate::depth::{DepthOutput, DepthProcessor};

/// How long a depth run waits for the accelerator before skipping the frame.
pub const DEPTH_PERMIT_WAIT: Duration = Duration::from_millis(1_000);

/// How often a waiting depth run looks at the accelerator again.
const PERMIT_POLL: Duration = Duration::from_millis(5);

/// What the depth layer can do.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DepthModelState {
    /// No depth model is configured or installed.
    #[default]
    NoModel,
    /// The model is loaded and its last run (if any) worked.
    Ready,
    /// The model did not load, or its last run failed, with why.
    Unavailable(String),
}

/// The depth layer's configuration, set once at start.
#[derive(Debug, Clone)]
pub struct DepthSetup {
    /// The registered depth model, `None` when there is none.
    pub model_id: Option<String>,
    /// Why there is no usable model, when there is none or it did not load.
    pub unavailable: Option<String>,
    /// The shortest gap between two runs on one camera.
    pub interval_ms: u64,
    /// Each camera's mount and field of view.
    pub geometries: HashMap<String, CameraGeometry>,
}

/// The depth layer's live state.
#[derive(Debug, Default)]
pub(super) struct DepthRuntime {
    model_id: Option<String>,
    state: DepthModelState,
    interval_ms: u64,
    processor: DepthProcessor,
}

/// Counts a waiter for the accelerator while it lives, so a depth run can
/// see that higher-priority inference is queued.
pub(super) struct PriorityWaiter<'a>(&'a AtomicUsize);

impl<'a> PriorityWaiter<'a> {
    pub(super) fn new(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, Ordering::AcqRel);
        Self(count)
    }
}

impl Drop for PriorityWaiter<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl VisionEngine {
    fn depth_runtime(&self) -> std::sync::MutexGuard<'_, DepthRuntime> {
        self.depth
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Configure the depth layer (once, at start).
    pub fn configure_depth(&self, setup: DepthSetup) {
        let mut rt = self.depth_runtime();
        rt.state = match (&setup.model_id, setup.unavailable) {
            (_, Some(reason)) => DepthModelState::Unavailable(reason),
            (Some(_), None) => DepthModelState::Ready,
            (None, None) => DepthModelState::NoModel,
        };
        rt.model_id = setup.model_id;
        rt.interval_ms = setup.interval_ms;
        rt.processor = DepthProcessor::new(setup.geometries);
    }

    /// The depth model's state.
    pub fn depth_model_state(&self) -> DepthModelState {
        self.depth_runtime().state.clone()
    }

    /// Whether a depth model is loaded (a lease then runs it).
    pub fn depth_model_ready(&self) -> bool {
        self.depth_runtime().state == DepthModelState::Ready
    }

    /// The shortest gap between two depth runs on one camera.
    pub fn depth_interval_ms(&self) -> u64 {
        self.depth_runtime().interval_ms
    }

    /// The ranging the depth fit uses; the feed readers write it.
    pub fn range_feed(&self) -> Arc<crate::depth::feed::RangeFeed> {
        self.range_feed.clone()
    }

    /// Subscribe to the published depth records.
    pub fn subscribe_depth(&self) -> broadcast::Receiver<DepthRecord> {
        self.depth_tx.subscribe()
    }

    /// Publish one depth record to every subscriber.
    pub fn publish_depth(&self, record: DepthRecord) {
        let _ = self.depth_tx.send(record);
    }

    /// Take the accelerator for a depth run: only while nothing of higher
    /// priority waits for it, giving up after [`DEPTH_PERMIT_WAIT`].
    async fn acquire_after_priority(&self) -> Option<SemaphorePermit<'_>> {
        let deadline = Instant::now() + DEPTH_PERMIT_WAIT;
        loop {
            if self.priority_waiters.load(Ordering::Acquire) == 0 {
                if let Ok(permit) = self.accel_lease.try_acquire() {
                    return Some(permit);
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(PERMIT_POLL).await;
        }
    }

    /// Run the depth model on one published frame and publish what the
    /// camera's leases want: the grid while a depth lease is live, the
    /// landing zones (at most once a second) while a surfaces lease is.
    /// Returns `Ok(None)` when nothing was wanted, no model is ready, or the
    /// accelerator stayed busy.
    pub async fn run_depth(
        &self,
        desc: &FrameDescriptor,
        frame: &[u8],
        now_ms: u64,
    ) -> Result<Option<DepthOutput>> {
        let (want_depth, want_surfaces) = self.depth_wanted(&desc.camera_id, now_ms);
        let model_id = {
            let rt = self.depth_runtime();
            match (&rt.model_id, &rt.state) {
                (Some(id), DepthModelState::Ready | DepthModelState::Unavailable(_)) => id.clone(),
                _ => return Ok(None),
            }
        };
        if !(want_depth || want_surfaces) {
            return Ok(None);
        }
        let Some(permit) = self.acquire_after_priority().await else {
            tracing::debug!(camera = %desc.camera_id, "depth_run_skipped_accelerator_busy");
            return Ok(None);
        };
        let loaded = {
            let models = self.models.lock().await;
            models.get(&model_id).and_then(|m| m.loaded.clone())
        };
        let Some(loaded) = loaded else {
            self.depth_runtime().state = DepthModelState::Unavailable(format!(
                "depth model {model_id} has no loaded backend"
            ));
            return Ok(None);
        };
        let t0 = Instant::now();
        let result =
            run_blocking(|| loaded.infer_dense(frame, desc.width, desc.height, desc.format));
        drop(permit);
        let latency_ms = t0.elapsed().as_secs_f32() * 1000.0;
        let map = match result {
            Ok(map) => map,
            Err(e) => {
                self.depth_runtime().state = DepthModelState::Unavailable(format!("{e:#}"));
                return Err(e);
            }
        };
        self.record_timing(&model_id, latency_ms, now_ms as i64)
            .await;
        let ranging = self.range_feed.snapshot(now_ms);
        let aspect = desc.width as f32 / desc.height.max(1) as f32;
        let out = {
            let mut rt = self.depth_runtime();
            rt.state = DepthModelState::Ready;
            rt.processor.process(
                &desc.camera_id,
                &map,
                aspect,
                desc.ts_ns,
                now_ms,
                &ranging,
                want_surfaces,
            )
        };
        if want_depth {
            self.publish_depth(DepthRecord::Depth(out.grid.clone()));
        }
        if let Some(s) = &out.surfaces {
            self.publish_depth(DepthRecord::Surfaces(s.clone()));
        }
        Ok(Some(out))
    }

    /// The status sidecar's `depth` block for `cameras` (plus any leased).
    pub fn depth_status(&self, cameras: &[String], now_ms: u64) -> DepthStatus {
        let leases = self.depth_leases(cameras, now_ms);
        let rt = self.depth_runtime();
        let (state, reason) = match &rt.state {
            DepthModelState::NoModel => {
                ("no_model", Some("no depth model is installed".to_string()))
            }
            DepthModelState::Ready => ("ready", None),
            DepthModelState::Unavailable(r) => ("unavailable", Some(r.clone())),
        };
        let cameras: BTreeMap<String, CameraDepthStatus> = leases
            .into_iter()
            .map(|(id, held, depth, surfaces)| {
                let c = rt.processor.counters(&id);
                (
                    id,
                    CameraDepthStatus {
                        depth,
                        surfaces,
                        leases: held,
                        scale_source: c.scale_source.map(|s| s.as_str().to_string()),
                        runs: c.runs,
                    },
                )
            })
            .collect();
        DepthStatus {
            state: state.to_string(),
            model_id: rt.model_id.clone(),
            reason,
            interval_ms: rt.interval_ms,
            cameras,
        }
    }
}
