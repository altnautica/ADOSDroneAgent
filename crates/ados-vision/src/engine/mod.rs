//! The vision engine core.
//!
//! Owns the moving parts the socket server and the capture tasks share:
//!
//! - a per-camera [`crate::ring::RingWriter`] plus a published-descriptor
//!   broadcast (subscribers on `vision.sock` receive every descriptor),
//! - the model registry (engine-run models carry a loaded backend model;
//!   plugin-side models are recorded only),
//! - the accelerator lease arbiter — a single permit serializing inference on
//!   the shared NPU so two models never run on it at once,
//! - the detection broadcast every published [`DetectionBatch`] fans out on.
//!
//! The engine is wrapped in an `Arc` and shared by the capture tasks (which
//! write frames and publish descriptors) and the socket server (which registers
//! models, runs inference, and publishes detections).

use std::collections::HashMap;
use std::sync::Arc;

use ados_protocol::framebus::{
    Detection, DetectionBatch, FrameDescriptor, FrameFormat, ModelExecution, ModelMetadata,
    VISION_DETECTION_VERSION,
};
use ados_protocol::vision_rpc::owned_model_id;
use anyhow::{anyhow, Result};
use tokio::sync::{broadcast, Mutex, Semaphore};

use crate::backend::{LoadedModel, VisionBackend};
use crate::ring::{now_ms, RingWriter};
mod models;
mod tracking;

use crate::tracker::{Appearance, Candidate, SingleObjectTracker, TrackerConfig};

/// Broadcast depth for frame descriptors and detections. Slow subscribers lag
/// and skip rather than back up the publisher (latest-wins, like the rings).
const BROADCAST_DEPTH: usize = 64;

/// The most models one plugin may hold registered at once. Each engine-run
/// registration can load a backend session, so an uncapped plugin could grow
/// the registry and the accelerator's memory without bound.
pub const MAX_MODELS_PER_OWNER: usize = 8;

/// A registered model: its metadata, the plugin that registered it (`None` for
/// the engine's own configured models) and, for engine-run models, the loaded
/// backend model that runs it. Plugin-side models have no loaded model (the
/// plugin runs them itself). The loaded model is shared so an inference can
/// run on it after the registry lock is released.
struct RegisteredModel {
    meta: ModelMetadata,
    owner: Option<String>,
    loaded: Option<Arc<dyn LoadedModel>>,
}

/// Run a blocking model call (a sidecar round trip, an accelerator session)
/// without stalling the async runtime. On a multi-thread runtime the worker
/// hands its queued tasks to the other workers first; on a current-thread
/// runtime (tests) the call runs in place. The borrowed frame is used as is,
/// with no copy.
fn run_blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

/// One camera's published surface: the ring writer it writes frames into.
struct CameraRing {
    writer: RingWriter,
}

/// Rolling per-model inference timing for the telemetry surface. EWMA-smoothed so
/// one slow frame does not spike the reported latency/fps. All-zero until the
/// model runs (the honest "no data" reading, never a claim of zero throughput).
#[derive(Debug, Clone, Copy, Default)]
struct ModelTiming {
    ewma_latency_ms: f32,
    ewma_interval_ms: f32,
    last_infer_ms: Option<i64>,
    samples: u64,
}

impl ModelTiming {
    /// EWMA weight for a new sample (a light smoothing so the reading tracks the
    /// current rate without jitter).
    const ALPHA: f32 = 0.2;

    fn record(&mut self, latency_ms: f32, now: i64) {
        self.ewma_latency_ms = if self.samples == 0 {
            latency_ms
        } else {
            Self::ALPHA * latency_ms + (1.0 - Self::ALPHA) * self.ewma_latency_ms
        };
        if let Some(last) = self.last_infer_ms {
            let interval = (now - last).max(0) as f32;
            if interval > 0.0 {
                self.ewma_interval_ms = if self.ewma_interval_ms == 0.0 {
                    interval
                } else {
                    Self::ALPHA * interval + (1.0 - Self::ALPHA) * self.ewma_interval_ms
                };
            }
        }
        self.last_infer_ms = Some(now);
        self.samples = self.samples.saturating_add(1);
    }

    fn fps(&self) -> f32 {
        if self.ewma_interval_ms > 0.0 {
            1000.0 / self.ewma_interval_ms
        } else {
            0.0
        }
    }
}

/// The shared engine state.
pub struct VisionEngine {
    backend: Box<dyn VisionBackend>,
    cameras: Mutex<HashMap<String, CameraRing>>,
    models: Mutex<HashMap<String, RegisteredModel>>,
    /// Single permit ⇒ inference on the shared accelerator is serialized.
    accel_lease: Semaphore,
    frame_tx: broadcast::Sender<FrameDescriptor>,
    detection_tx: broadcast::Sender<DetectionBatch>,
    /// How camera rings are sized: the requested depth and the hard ceiling on
    /// one ring's shared-memory footprint.
    sizing: RingSizing,
    /// Per-camera single-object tracker. Built lazily on the first detection or
    /// an operator designation for a camera. Only consulted when
    /// `tracker_enabled`.
    trackers: Mutex<HashMap<String, SingleObjectTracker>>,
    /// When true, `infer_and_publish` runs the per-camera tracker between
    /// inference and publish so the published batch carries a stable `track_id`
    /// + `lock_state` on the locked object. Default false ⇒ raw detections.
    tracker_enabled: bool,
    /// Tuning the per-camera trackers are built with.
    tracker_cfg: TrackerConfig,
    /// When true (and a `reid_model_id` is registered + loaded), the tracker is
    /// fed a learned appearance embedding per detection so it re-identifies the
    /// locked subject across a distractor crossing or a brief occlusion, not by
    /// motion alone. Off ⇒ the long-standing motion-only association.
    reid_enabled: bool,
    /// The registered model id whose `embed` produces the appearance embedding.
    reid_model_id: Option<String>,
    /// Per-model rolling inference timing (fps + latency), fed by `infer` and read
    /// by `list_models` for the telemetry surface.
    timings: Mutex<HashMap<String, ModelTiming>>,
}

/// How a camera ring is sized.
///
/// Two independent bounds, deliberately separate: `slot_count` is the recycling
/// depth an operator asks for, and `budget_bytes` is the ceiling on what one
/// ring may occupy in `/dev/shm`. The depth alone cannot bound the memory,
/// because the slot size is decided at runtime by the camera's frame size — the
/// engine reduces the depth to fit the budget, and refuses the camera outright
/// when a single frame cannot fit.
#[derive(Debug, Clone, Copy)]
pub struct RingSizing {
    pub slot_count: u32,
    pub budget_bytes: usize,
}

impl RingSizing {
    pub fn new(slot_count: u32, budget_bytes: usize) -> Self {
        Self {
            slot_count,
            budget_bytes,
        }
    }
}

/// A bare depth carries the default budget, so a caller that has no opinion on
/// the memory ceiling still gets one.
impl From<u32> for RingSizing {
    fn from(slot_count: u32) -> Self {
        Self::new(slot_count, crate::ring::DEFAULT_RING_BUDGET_BYTES)
    }
}

impl VisionEngine {
    /// Build the engine around a chosen backend. The tracker is off: the engine
    /// publishes raw detections (the long-standing behaviour).
    pub fn new(backend: Box<dyn VisionBackend>, sizing: impl Into<RingSizing>) -> Arc<Self> {
        Self::with_tracker(backend, sizing, false, TrackerConfig::default())
    }

    /// Build the engine with the per-camera tracker explicitly enabled or
    /// disabled. When enabled, `infer_and_publish` runs a single-object tracker
    /// per camera and stamps the locked object's `track_id` + `lock_state` onto
    /// the published batch; when disabled the behaviour is identical to [`new`].
    ///
    /// [`new`]: Self::new
    pub fn with_tracker(
        backend: Box<dyn VisionBackend>,
        sizing: impl Into<RingSizing>,
        tracker_enabled: bool,
        tracker_cfg: TrackerConfig,
    ) -> Arc<Self> {
        Self::with_tracker_reid(backend, sizing, tracker_enabled, tracker_cfg, false, None)
    }

    /// Build the engine with the tracker and the learned-appearance (re-id) path
    /// configured. When `reid_enabled` and `reid_model_id` names a registered,
    /// loaded model, the tracker associates on the model's appearance embedding
    /// (plus motion); otherwise it is motion-only. The re-id model is registered
    /// separately (a second `register_model`), so a missing/failed re-id model
    /// degrades cleanly to motion-only rather than rejecting the build.
    pub fn with_tracker_reid(
        backend: Box<dyn VisionBackend>,
        sizing: impl Into<RingSizing>,
        tracker_enabled: bool,
        tracker_cfg: TrackerConfig,
        reid_enabled: bool,
        reid_model_id: Option<String>,
    ) -> Arc<Self> {
        let (frame_tx, _) = broadcast::channel(BROADCAST_DEPTH);
        let (detection_tx, _) = broadcast::channel(BROADCAST_DEPTH);
        let mut sizing = sizing.into();
        sizing.slot_count = sizing.slot_count.max(crate::ring::MIN_SLOT_COUNT);
        Arc::new(Self {
            backend,
            cameras: Mutex::new(HashMap::new()),
            models: Mutex::new(HashMap::new()),
            accel_lease: Semaphore::new(1),
            frame_tx,
            detection_tx,
            sizing,
            trackers: Mutex::new(HashMap::new()),
            tracker_enabled,
            tracker_cfg,
            reid_enabled,
            reid_model_id,
            timings: Mutex::new(HashMap::new()),
        })
    }

    /// Record one inference's timing for a model (called by `infer`). Locks only
    /// the timings map, after the models lock is released, so the lock order stays
    /// models → timings.
    async fn record_timing(&self, model_id: &str, latency_ms: f32, now: i64) {
        let mut timings = self.timings.lock().await;
        timings
            .entry(model_id.to_string())
            .or_default()
            .record(latency_ms, now);
    }

    /// The backend name (for logs and the socket info reply).
    pub fn backend_name(&self) -> &str {
        self.backend.name()
    }

    /// Whether the engine's loaded backend runs real inference (not the mock).
    /// The same fact [`Self::list_models`] folds into each model's
    /// `is_inference_capable`, exposed standalone so a caller can ask before —
    /// or without ever — registering a model. The `/api/status` perception-tier
    /// honesty check needs exactly this: whether `select_backend` actually
    /// resolved to a real backend on this build/board, independent of whether
    /// vision has any model configured.
    pub fn is_inference_capable(&self) -> bool {
        self.backend.is_inference_capable()
    }

    /// Subscribe to published frame descriptors. Used by `vision.sock`
    /// `subscribe_frames` to stream descriptors to a plugin.
    pub fn subscribe_frames(&self) -> broadcast::Receiver<FrameDescriptor> {
        self.frame_tx.subscribe()
    }

    /// Subscribe to published detection batches.
    pub fn subscribe_detections(&self) -> broadcast::Receiver<DetectionBatch> {
        self.detection_tx.subscribe()
    }

    /// Ensure a ring exists for `camera_id`, sized for `width` x `height` in
    /// `format`. Re-sizes (recreates) the ring when a larger frame arrives.
    ///
    /// The depth is the configured one only while it fits the ring's byte
    /// budget: the slot size is not known until a frame arrives, so a depth
    /// that is fine for a 480p tap is gigabytes at 4K. Over budget the depth is
    /// reduced (a shallower ring still streams; an exhausted `/dev/shm` takes
    /// the box down), and a frame too large to hold two of is refused outright.
    async fn ensure_ring(
        &self,
        camera_id: &str,
        width: u32,
        height: u32,
        format: FrameFormat,
    ) -> Result<()> {
        let needed = format.frame_bytes(width, height) as u32;
        let mut cams = self.cameras.lock().await;
        let recreate = match cams.get(camera_id) {
            Some(cr) => cr.writer.layout().slot_bytes < needed,
            None => true,
        };
        if recreate {
            let budget = self.sizing.budget_bytes;
            let slots = crate::ring::fit_slot_count(self.sizing.slot_count, needed, budget)
                .ok_or_else(|| {
                    anyhow!(
                        "ring for {camera_id}: a {needed}-byte frame does not fit \
                         {} slots in the {budget}-byte budget",
                        crate::ring::MIN_SLOT_COUNT
                    )
                })?;
            if slots < self.sizing.slot_count {
                tracing::warn!(
                    camera = camera_id,
                    requested = self.sizing.slot_count,
                    slots,
                    frame_bytes = needed,
                    budget_bytes = budget,
                    "ring depth reduced to fit the shared-memory budget"
                );
            }
            let layout =
                ados_protocol::framebus::RingLayout::for_frame(slots, width, height, format);
            // Retire the outgrown ring before creating its replacement. Both
            // open the same path, and the old writer's drop unlinks it: dropped
            // after, it would unlink the replacement's name, leaving a ring no
            // consumer can map. Dropped first, the name names a new file, which
            // a consumer holding the old mapping detects and remaps.
            cams.remove(camera_id);
            let shm_name = format!("{}{camera_id}", crate::ring::RING_NAME_PREFIX);
            let writer = RingWriter::open_or_create(&shm_name, layout, budget)
                .map_err(|e| anyhow!("ring open for {camera_id}: {e}"))?;
            cams.insert(camera_id.to_string(), CameraRing { writer });
        }
        Ok(())
    }

    /// Write one captured frame into the camera's ring and publish its
    /// descriptor on `vision.frame`. Returns the descriptor.
    #[allow(clippy::too_many_arguments)]
    pub async fn publish_frame(
        &self,
        camera_id: &str,
        frame_id: u64,
        ts_ms: i64,
        width: u32,
        height: u32,
        format: FrameFormat,
        data: &[u8],
    ) -> Result<FrameDescriptor> {
        self.ensure_ring(camera_id, width, height, format).await?;
        let desc = {
            let mut cams = self.cameras.lock().await;
            let cr = cams
                .get_mut(camera_id)
                .ok_or_else(|| anyhow!("ring vanished for {camera_id}"))?;
            cr.writer
                .write_frame(camera_id, frame_id, ts_ms, width, height, format, data)
                .map_err(|e| anyhow!("write frame for {camera_id}: {e}"))?
        };
        // A send error just means no subscribers; that is fine.
        let _ = self.frame_tx.send(desc.clone());
        Ok(desc)
    }

    /// Read the pixels a descriptor this engine published names, out of the
    /// camera's ring through the per-slot seqlock. The engine writes the ring,
    /// so it reads its own region rather than mapping the file. Errors when the
    /// camera has no ring, the descriptor names a ring other than the camera's
    /// current one, or the slot has been recycled since.
    pub async fn read_frame(&self, desc: &FrameDescriptor) -> Result<Vec<u8>> {
        let cams = self.cameras.lock().await;
        let cr = cams
            .get(&desc.camera_id)
            .ok_or_else(|| anyhow!("no frame ring for camera {}", desc.camera_id))?;
        if cr.writer.shm_name() != desc.shm_name {
            return Err(anyhow!(
                "ring {} is not camera {}'s ring",
                desc.shm_name,
                desc.camera_id
            ));
        }
        cr.writer
            .read_frame(desc.slot, desc.seq)
            .map_err(|e| anyhow!("ring read: {e}"))?
            .ok_or_else(|| {
                anyhow!(
                    "ring slot {} no longer holds seq {} (torn/stale)",
                    desc.slot,
                    desc.seq
                )
            })
    }

    /// Whether `camera_id` has a frame ring, i.e. the engine captures it.
    pub async fn has_ring(&self, camera_id: &str) -> bool {
        self.cameras.lock().await.contains_key(camera_id)
    }
}

#[cfg(test)]
mod tests;
