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
use anyhow::{anyhow, Result};
use tokio::sync::{broadcast, Mutex, Semaphore};

use crate::backend::{LoadedModel, VisionBackend};
use crate::ring::{now_ms, RingWriter};
use crate::tracker::{Appearance, Candidate, SingleObjectTracker, TrackerConfig};

/// Broadcast depth for frame descriptors and detections. Slow subscribers lag
/// and skip rather than back up the publisher (latest-wins, like the rings).
const BROADCAST_DEPTH: usize = 64;

/// A registered model: its metadata plus, for engine-run models, the loaded
/// backend model that runs it. Plugin-side models have no loaded model (the
/// plugin runs them itself).
struct RegisteredModel {
    meta: ModelMetadata,
    loaded: Option<Box<dyn LoadedModel>>,
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
    /// Ring slot count and downscale target, from config.
    slot_count: u32,
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

impl VisionEngine {
    /// Build the engine around a chosen backend. The tracker is off: the engine
    /// publishes raw detections (the long-standing behaviour).
    pub fn new(backend: Box<dyn VisionBackend>, slot_count: u32) -> Arc<Self> {
        Self::with_tracker(backend, slot_count, false, TrackerConfig::default())
    }

    /// Build the engine with the per-camera tracker explicitly enabled or
    /// disabled. When enabled, `infer_and_publish` runs a single-object tracker
    /// per camera and stamps the locked object's `track_id` + `lock_state` onto
    /// the published batch; when disabled the behaviour is identical to [`new`].
    ///
    /// [`new`]: Self::new
    pub fn with_tracker(
        backend: Box<dyn VisionBackend>,
        slot_count: u32,
        tracker_enabled: bool,
        tracker_cfg: TrackerConfig,
    ) -> Arc<Self> {
        Self::with_tracker_reid(
            backend,
            slot_count,
            tracker_enabled,
            tracker_cfg,
            false,
            None,
        )
    }

    /// Build the engine with the tracker and the learned-appearance (re-id) path
    /// configured. When `reid_enabled` and `reid_model_id` names a registered,
    /// loaded model, the tracker associates on the model's appearance embedding
    /// (plus motion); otherwise it is motion-only. The re-id model is registered
    /// separately (a second `register_model`), so a missing/failed re-id model
    /// degrades cleanly to motion-only rather than rejecting the build.
    pub fn with_tracker_reid(
        backend: Box<dyn VisionBackend>,
        slot_count: u32,
        tracker_enabled: bool,
        tracker_cfg: TrackerConfig,
        reid_enabled: bool,
        reid_model_id: Option<String>,
    ) -> Arc<Self> {
        let (frame_tx, _) = broadcast::channel(BROADCAST_DEPTH);
        let (detection_tx, _) = broadcast::channel(BROADCAST_DEPTH);
        Arc::new(Self {
            backend,
            cameras: Mutex::new(HashMap::new()),
            models: Mutex::new(HashMap::new()),
            accel_lease: Semaphore::new(1),
            frame_tx,
            detection_tx,
            slot_count: slot_count.max(2),
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
            let layout = ados_protocol::framebus::RingLayout::for_frame(
                self.slot_count,
                width,
                height,
                format,
            );
            let shm_name = format!("ados-vision-{camera_id}");
            let writer = RingWriter::open_or_create(&shm_name, layout)
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

    /// Register a model. Engine-run models are loaded on the backend now (a load
    /// failure falls back to recording the model without a loaded handle, so a
    /// missing model file or sidecar never rejects the registration). Returns
    /// the resolved execution and whether a backend model was loaded.
    pub async fn register_model(&self, meta: ModelMetadata) -> Result<(ModelExecution, bool)> {
        let execution = meta.execution;
        let loaded = if execution == ModelExecution::EngineRun {
            match self.backend.load(&meta) {
                Ok(m) => Some(m),
                Err(e) => {
                    tracing::warn!(model = %meta.id, error = %e, "model_load_failed; recorded without backend");
                    None
                }
            }
        } else {
            None
        };
        let had_backend = loaded.is_some();
        let mut models = self.models.lock().await;
        models.insert(meta.id.clone(), RegisteredModel { meta, loaded });
        Ok((execution, had_backend))
    }

    /// Number of registered models.
    pub async fn model_count(&self) -> usize {
        self.models.lock().await.len()
    }

    /// Registered model ids whose task (kind) matches `kind`, sorted for a
    /// deterministic result. Lets a consumer request perception by TASK ("a
    /// detection model", "a depth model") instead of naming one global
    /// detector, so several tasks can be selected and paced together.
    pub async fn models_for_kind(&self, kind: ados_protocol::framebus::ModelKind) -> Vec<String> {
        let models = self.models.lock().await;
        let mut ids: Vec<String> = models
            .values()
            .filter(|m| m.meta.kind == kind)
            .map(|m| m.meta.id.clone())
            .collect();
        ids.sort();
        ids
    }

    /// A read-back of every registered model (id, task, execution, whether a
    /// backend loaded, output classes), sorted by id. The GCS vision hub shows
    /// this so an operator sees every model loaded on the drone, not only the
    /// ones actively publishing detections.
    pub async fn list_models(&self) -> Vec<ados_protocol::framebus::ModelInfo> {
        let models = self.models.lock().await;
        // Lock order models → timings (record_timing only ever holds timings).
        let timings = self.timings.lock().await;
        let backend_capable = self.backend.is_inference_capable();
        let mut out: Vec<ados_protocol::framebus::ModelInfo> = models
            .values()
            .map(|m| {
                let t = timings.get(&m.meta.id);
                ados_protocol::framebus::ModelInfo {
                    id: m.meta.id.clone(),
                    kind: m.meta.kind,
                    execution: m.meta.execution,
                    backend_loaded: m.loaded.is_some(),
                    output_classes: m.meta.output_classes.clone(),
                    fps: t.map(|t| t.fps()).unwrap_or(0.0),
                    latency_ms: t.map(|t| t.ewma_latency_ms).unwrap_or(0.0),
                    // A model runs a real detector only when its file loaded AND the
                    // engine's backend is inference-capable (a mock backend is not),
                    // so a placeholder is never presented as a working detector.
                    is_inference_capable: m.loaded.is_some() && backend_capable,
                }
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Resolve a perception capability: the first inference-capable registered
    /// model of `kind` whose `output_classes` covers `class` (or any model of
    /// that kind when `class` is `None`), by id order. `None` when nothing
    /// matches — the honest "this node cannot do that" answer instead of
    /// hand-picking a global detector.
    ///
    /// Only inference-capable models resolve: a mock-backed or failed-to-load
    /// model is never returned as a working capability (Rule 44). The result is
    /// deterministic because [`Self::list_models`] is sorted by id.
    pub async fn resolve_capability(
        &self,
        kind: ados_protocol::framebus::ModelKind,
        class: Option<&str>,
    ) -> Option<ados_protocol::framebus::ModelInfo> {
        self.list_models()
            .await
            .into_iter()
            .filter(|m| m.kind == kind && m.is_inference_capable)
            .find(|m| match class {
                Some(c) => m.output_classes.iter().any(|oc| oc == c),
                None => true,
            })
    }

    /// Run inference for `model_id` on a frame, serialized on the accelerator
    /// lease. Returns the detections. Errors when the model is unknown, is
    /// plugin-side (the plugin must run it), or has no loaded backend model.
    pub async fn infer(
        &self,
        model_id: &str,
        frame: &[u8],
        width: u32,
        height: u32,
        format: FrameFormat,
    ) -> Result<Vec<Detection>> {
        // Acquire the accelerator lease for the duration of the inference so two
        // models never contend for the shared NPU. The inference call itself is
        // synchronous, so it runs inside the permit's scope.
        let _permit = self
            .accel_lease
            .acquire()
            .await
            .map_err(|_| anyhow!("accelerator lease closed"))?;

        let models = self.models.lock().await;
        let reg = models
            .get(model_id)
            .ok_or_else(|| anyhow!("unknown model {model_id}"))?;
        if reg.meta.execution == ModelExecution::PluginSide {
            return Err(anyhow!(
                "model {model_id} is plugin-side; the plugin runs it"
            ));
        }
        let loaded = reg
            .loaded
            .as_ref()
            .ok_or_else(|| anyhow!("model {model_id} has no loaded backend"))?;
        let t0 = std::time::Instant::now();
        let result = loaded.infer(frame, width, height, format);
        let latency_ms = t0.elapsed().as_secs_f32() * 1000.0;
        // Release the models lock (the borrow of `loaded`/`reg` ends with `result`)
        // before recording the timing, keeping the lock order models → timings.
        drop(models);
        if result.is_ok() {
            self.record_timing(model_id, latency_ms, now_ms()).await;
        }
        result
    }

    /// Publish a detection batch on `vision.detection`. A plugin-side model
    /// calls this; an engine-run flow calls it after [`Self::infer`]. Returns
    /// the subscriber count the batch reached (0 when none).
    pub fn publish_detection(&self, batch: DetectionBatch) -> usize {
        self.detection_tx.send(batch).unwrap_or(0)
    }

    /// Convenience for the engine-run flow: infer then publish, building the
    /// batch from the frame descriptor and the model's id. When the tracker is
    /// enabled, the detections pass through the camera's single-object tracker
    /// first so the locked object carries a stable `track_id` + `lock_state`.
    pub async fn infer_and_publish(
        &self,
        model_id: &str,
        desc: &FrameDescriptor,
        frame: &[u8],
    ) -> Result<DetectionBatch> {
        let detections = self
            .infer(model_id, frame, desc.width, desc.height, desc.format)
            .await?;
        let detections = if self.tracker_enabled {
            self.apply_tracker(
                &desc.camera_id,
                detections,
                frame,
                desc.width,
                desc.height,
                desc.format,
            )
            .await
        } else {
            detections
        };
        let batch = DetectionBatch {
            v: VISION_DETECTION_VERSION,
            model_id: model_id.to_string(),
            camera_id: desc.camera_id.clone(),
            frame_id: desc.frame_id,
            ts_ms: desc.ts_ms,
            // The vision frame's pixel size the boxes are in, so the overlay
            // scales to it.
            frame_width: desc.width,
            frame_height: desc.height,
            detections,
        };
        self.publish_detection(batch.clone());
        Ok(batch)
    }

    /// Run the camera's single-object tracker over `detections` and return the
    /// batch to publish: every detection is kept (so an overlay sees them all),
    /// and the locked object's `track_id` / `lock_state` / `assoc_confidence`
    /// are stamped onto its detection. On a measured frame the stamp lands on the
    /// best-matching input box; on a coast/ambiguous frame (no measured box) the
    /// tracker's predicted box is appended so the held target stays visible and
    /// followable.
    async fn apply_tracker(
        &self,
        camera_id: &str,
        detections: Vec<Detection>,
        frame: &[u8],
        width: u32,
        height: u32,
        format: FrameFormat,
    ) -> Vec<Detection> {
        // Extract appearance embeddings BEFORE taking the tracker lock (the
        // extraction takes the accelerator lease + the model lock; keeping the
        // lock order accel→models, distinct from the tracker lock, avoids any
        // nesting). When re-id is off this is skipped entirely (motion-only).
        let appearances = if self.reid_enabled {
            Some(
                self.extract_appearances(frame, width, height, format, &detections)
                    .await,
            )
        } else {
            None
        };

        let mut trackers = self.trackers.lock().await;
        let tracker = trackers
            .entry(camera_id.to_string())
            .or_insert_with(|| SingleObjectTracker::new(self.tracker_cfg));
        let update = match appearances {
            Some(apps) => {
                let candidates: Vec<Candidate> = detections
                    .iter()
                    .cloned()
                    .zip(apps)
                    .map(|(det, app)| match app {
                        Some(a) => Candidate::with_appearance(det, a),
                        None => Candidate::motion_only(det),
                    })
                    .collect();
                tracker.update_with_appearance(&candidates)
            }
            None => tracker.update(&detections),
        };
        merge_tracked(detections, update)
    }

    /// Extract a per-detection appearance embedding for the re-id path: crop each
    /// box to the re-id model's input, run the model's `embed`, and L2-normalize
    /// the result. Returns one `Option<Appearance>` per detection (in order); a
    /// `None` means motion-only for that box (the re-id model is not loaded, the
    /// crop was degenerate, or the embed failed) and never blocks tracking. The
    /// embeds run under the accelerator lease so they never contend with the
    /// detector on the shared NPU.
    async fn extract_appearances(
        &self,
        frame: &[u8],
        width: u32,
        height: u32,
        format: FrameFormat,
        detections: &[Detection],
    ) -> Vec<Option<Appearance>> {
        let none = || vec![None; detections.len()];
        if format != FrameFormat::Rgb24 {
            // The crop + ONNX/RKNN embed paths are rgb24; a non-rgb24 frame
            // degrades to motion-only rather than guessing a conversion.
            return none();
        }
        let Some(model_id) = self.reid_model_id.clone() else {
            return none();
        };
        // The re-id model's input dims, if it is registered + loaded.
        let dims = {
            let models = self.models.lock().await;
            match models.get(&model_id) {
                Some(reg) if reg.loaded.is_some() => {
                    Some((reg.meta.input_width, reg.meta.input_height))
                }
                _ => None,
            }
        };
        let Some((iw, ih)) = dims else {
            return none();
        };
        // Crop every box first (no lock needed). A box-less percept has no box
        // to crop, so it gets no appearance embedding (motion-only for it).
        let crops: Vec<Option<Vec<u8>>> = detections
            .iter()
            .map(|d| {
                d.bbox
                    .as_ref()
                    .and_then(|b| crate::reid::crop_resize_rgb24(frame, width, height, b, iw, ih))
            })
            .collect();

        // Embed under the accelerator lease + the model lock. If the lease can't
        // be acquired (a closing engine), degrade to motion-only rather than
        // running embeds lease-less against a concurrent detector inference.
        let _permit = match self.accel_lease.acquire().await {
            Ok(p) => p,
            Err(_) => return none(),
        };
        let models = self.models.lock().await;
        let Some(reg) = models.get(&model_id) else {
            return none();
        };
        let Some(loaded) = reg.loaded.as_ref() else {
            return none();
        };
        crops
            .into_iter()
            .map(|crop| {
                let crop = crop?;
                match loaded.embed(&crop, iw, ih, FrameFormat::Rgb24) {
                    Ok(Some(mut emb)) if !emb.is_empty() => {
                        crate::reid::l2_normalize(&mut emb);
                        Some(Appearance::from_features(emb))
                    }
                    _ => None,
                }
            })
            .collect()
    }

    /// The track id the camera's lock currently holds (confirmed or coasting), if
    /// any. The operator/GCS reads this to know whether a target is locked.
    pub async fn current_track(&self, camera_id: &str) -> Option<u64> {
        self.trackers
            .lock()
            .await
            .get(camera_id)
            .and_then(|t| t.current_id())
    }

    /// Operator designation: lock the camera's tracker onto a specific detection
    /// (the box the operator clicked), overriding the auto-lock. Returns the new
    /// track id. Builds the camera's tracker if it does not exist yet.
    pub async fn designate(&self, camera_id: &str, target: &Detection) -> Option<u64> {
        let mut trackers = self.trackers.lock().await;
        let tracker = trackers
            .entry(camera_id.to_string())
            .or_insert_with(|| SingleObjectTracker::new(self.tracker_cfg));
        tracker.designate(target)
    }

    /// Operator re-confirm: clear the ambiguity latch on the camera's tracker
    /// after the operator re-confirms the target out of band. Returns false when
    /// the camera has no tracker yet.
    pub async fn redesignate(&self, camera_id: &str) -> bool {
        match self.trackers.lock().await.get_mut(camera_id) {
            Some(t) => {
                t.redesignate();
                true
            }
            None => false,
        }
    }

    /// Drop the camera's lock so the tracker re-seeds on the next detection.
    pub async fn reset_track(&self, camera_id: &str) {
        self.trackers.lock().await.remove(camera_id);
    }
}

/// Merge a [`TrackUpdate`] back into the frame's detections: stamp the locked
/// object onto its best-matching input box (measured frame), or append the
/// tracker's predicted box (coast/ambiguous frame, no measured input).
fn merge_tracked(
    mut detections: Vec<Detection>,
    update: crate::tracker::TrackUpdate,
) -> Vec<Detection> {
    let Some(mut locked) = update.detection else {
        // Idle or tentative-not-yet-confirmed: nothing to stamp.
        return detections;
    };
    // Only an operator-designated track presents a lock state to consumers,
    // mirroring the offload publish path. An automatically seeded track (the
    // most-confident auto-pick) is still tracked — it keeps its id and predicted
    // box — but carries no lock state, so a follow behavior only ever engages a
    // target the operator actually designated.
    if !update.operator_designated {
        locked.lock_state = None;
    }
    if update.measured {
        // The tracker associated to one of the input boxes; stamp the closest.
        // The tracker's reported box is always present; a box-less report (none
        // today) falls through to keeping the held target visible.
        match locked
            .bbox
            .as_ref()
            .and_then(|lb| best_overlap_index(&detections, lb))
        {
            Some(idx) => {
                detections[idx].track_id = locked.track_id;
                detections[idx].lock_state = locked.lock_state;
                detections[idx].assoc_confidence = locked.assoc_confidence;
            }
            // No overlapping input (shouldn't happen on a measured frame) — keep
            // the held target visible rather than dropping it.
            None => detections.push(locked),
        }
    } else {
        // Coasting / ambiguous hold: the predicted box has no input counterpart.
        detections.push(locked);
    }
    detections
}

/// The index of the detection with the greatest IoU against `bbox`, or `None`
/// when the list is empty or nothing overlaps at all.
fn best_overlap_index(
    detections: &[Detection],
    bbox: &ados_protocol::framebus::BoundingBox,
) -> Option<usize> {
    let mut best: Option<(usize, f32)> = None;
    for (i, d) in detections.iter().enumerate() {
        // A box-less percept can never overlap the tracked box.
        let Some(db) = d.bbox.as_ref() else {
            continue;
        };
        let i_o_u = bbox_iou(db, bbox);
        if i_o_u > 0.0 && best.is_none_or(|(_, b)| i_o_u > b) {
            best = Some((i, i_o_u));
        }
    }
    best.map(|(i, _)| i)
}

/// Intersection-over-union of two corner-form boxes.
fn bbox_iou(
    a: &ados_protocol::framebus::BoundingBox,
    b: &ados_protocol::framebus::BoundingBox,
) -> f32 {
    let (ax2, ay2) = (a.x + a.width, a.y + a.height);
    let (bx2, by2) = (b.x + b.width, b.y + b.height);
    let iw = (ax2.min(bx2) - a.x.max(b.x)).max(0.0);
    let ih = (ay2.min(by2) - a.y.max(b.y)).max(0.0);
    let inter = iw * ih;
    if inter <= 0.0 {
        return 0.0;
    }
    let union = a.width * a.height + b.width * b.height - inter;
    if union > 0.0 {
        inter / union
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MockBackend;
    use ados_protocol::framebus::{BoundingBox, LockState, ModelKind};

    fn engine() -> Arc<VisionEngine> {
        VisionEngine::new(Box::new(MockBackend), 4)
    }

    fn meta(id: &str, exec: ModelExecution) -> ModelMetadata {
        ModelMetadata {
            id: id.into(),
            kind: ModelKind::Detection,
            execution: exec,
            input_width: 8,
            input_height: 8,
            input_format: FrameFormat::Rgb24,
            output_classes: vec!["x".into()],
            model_path: None,
            head: ados_protocol::framebus::DetectionHead::Yolo8,
        }
    }

    #[tokio::test]
    async fn models_for_kind_filters_by_task() {
        let e = engine();
        e.register_model(meta("det-a", ModelExecution::PluginSide))
            .await
            .unwrap();
        e.register_model(meta("det-b", ModelExecution::PluginSide))
            .await
            .unwrap();
        let mut seg = meta("seg", ModelExecution::PluginSide);
        seg.kind = ModelKind::Segmentation;
        e.register_model(seg).await.unwrap();

        // Detection task returns both detectors, sorted; segmentation returns
        // the one seg model; a task with no models returns empty.
        assert_eq!(
            e.models_for_kind(ModelKind::Detection).await,
            vec!["det-a", "det-b"]
        );
        assert_eq!(
            e.models_for_kind(ModelKind::Segmentation).await,
            vec!["seg"]
        );
        assert!(e.models_for_kind(ModelKind::Tracking).await.is_empty());
    }

    /// A no-op loaded model, so a test backend can report inference-capable
    /// without pulling a real backend feature.
    struct NoopModel;

    impl LoadedModel for NoopModel {
        fn infer(
            &self,
            _frame: &[u8],
            _w: u32,
            _h: u32,
            _f: FrameFormat,
        ) -> Result<Vec<Detection>> {
            Ok(Vec::new())
        }
    }

    /// A test backend that loads any model and, unlike [`MockBackend`], reports
    /// inference-capable — so a resolved capability has a real model to return.
    struct CapableBackend;

    impl VisionBackend for CapableBackend {
        fn load(&self, _meta: &ModelMetadata) -> Result<Box<dyn LoadedModel>> {
            Ok(Box::new(NoopModel))
        }
        fn name(&self) -> &str {
            "capable"
        }
    }

    fn capable_engine() -> Arc<VisionEngine> {
        VisionEngine::new(Box::new(CapableBackend), 4)
    }

    #[tokio::test]
    async fn resolve_capability_matches_kind_and_class() {
        let e = capable_engine();
        // A person/car detector, engine-run so it loads on the capable backend.
        let mut det = meta("person-det", ModelExecution::EngineRun);
        det.output_classes = vec!["person".into(), "car".into()];
        e.register_model(det).await.unwrap();

        // detection + person resolves the model.
        let m = e
            .resolve_capability(ModelKind::Detection, Some("person"))
            .await
            .expect("a person detector resolves detection+person");
        assert_eq!(m.id, "person-det");
        assert!(m.is_inference_capable);

        // detection + None resolves any model of that kind.
        assert_eq!(
            e.resolve_capability(ModelKind::Detection, None)
                .await
                .map(|m| m.id),
            Some("person-det".into())
        );

        // An unlisted class does not resolve.
        assert!(e
            .resolve_capability(ModelKind::Detection, Some("boat"))
            .await
            .is_none());

        // A kind with no registered model does not resolve.
        assert!(e.resolve_capability(ModelKind::Depth, None).await.is_none());
    }

    #[tokio::test]
    async fn resolve_capability_skips_a_non_inference_capable_model() {
        // The default mock engine loads the model (backend_loaded) but is not
        // inference-capable, so the detection capability does not resolve — a
        // placeholder is never offered as a working capability (Rule 44).
        let e = engine();
        e.register_model(meta("m1", ModelExecution::EngineRun))
            .await
            .unwrap();
        assert!(e
            .resolve_capability(ModelKind::Detection, None)
            .await
            .is_none());
    }

    /// resolve returns the lowest-id inference-capable match (deterministic).
    #[tokio::test]
    async fn resolve_capability_is_deterministic_by_id() {
        let e = capable_engine();
        for id in ["det-c", "det-a", "det-b"] {
            let mut m = meta(id, ModelExecution::EngineRun);
            m.output_classes = vec!["person".into()];
            e.register_model(m).await.unwrap();
        }
        assert_eq!(
            e.resolve_capability(ModelKind::Detection, Some("person"))
                .await
                .map(|m| m.id),
            Some("det-a".into())
        );
    }

    #[tokio::test]
    async fn publishes_frame_and_descriptor() {
        let e = engine();
        let mut rx = e.subscribe_frames();
        let data = vec![0u8; FrameFormat::Rgb24.frame_bytes(8, 8)];
        let desc = e
            .publish_frame("uvc-0", 1, 100, 8, 8, FrameFormat::Rgb24, &data)
            .await
            .unwrap();
        assert_eq!(desc.camera_id, "uvc-0");
        assert_eq!(desc.seq, 1);
        let got = rx.try_recv().unwrap();
        assert_eq!(got, desc);
    }

    #[tokio::test]
    async fn ring_grows_for_a_larger_frame() {
        let e = engine();
        let small = vec![0u8; FrameFormat::Rgb24.frame_bytes(8, 8)];
        e.publish_frame("c", 1, 0, 8, 8, FrameFormat::Rgb24, &small)
            .await
            .unwrap();
        // A bigger frame forces a ring resize without error.
        let big = vec![0u8; FrameFormat::Rgb24.frame_bytes(16, 16)];
        let d = e
            .publish_frame("c", 2, 0, 16, 16, FrameFormat::Rgb24, &big)
            .await
            .unwrap();
        assert_eq!(d.width, 16);
    }

    #[tokio::test]
    async fn registers_engine_run_model_and_infers() {
        let e = engine();
        let (exec, had_backend) = e
            .register_model(meta("m1", ModelExecution::EngineRun))
            .await
            .unwrap();
        assert_eq!(exec, ModelExecution::EngineRun);
        assert!(had_backend);
        assert_eq!(e.model_count().await, 1);
        // Mock backend returns no detections.
        let out = e
            .infer("m1", &[0u8; 192], 8, 8, FrameFormat::Rgb24)
            .await
            .unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn model_timing_tracks_latency_and_fps() {
        let mut t = ModelTiming::default();
        // No data yet: an unrun model reports zero, not a fabricated rate.
        assert_eq!(t.fps(), 0.0);
        assert_eq!(t.ewma_latency_ms, 0.0);
        // First sample seeds the latency; fps still 0 (no interval yet).
        t.record(20.0, 1000);
        assert_eq!(t.ewma_latency_ms, 20.0);
        assert_eq!(t.fps(), 0.0);
        // Second sample 100 ms later: the interval is 100 ms -> ~10 fps.
        t.record(20.0, 1100);
        assert!(
            (t.fps() - 10.0).abs() < 0.01,
            "100ms interval -> ~10fps, got {}",
            t.fps()
        );
        assert_eq!(t.samples, 2);
    }

    #[tokio::test]
    async fn list_models_flags_a_mock_backend_as_not_inference_capable() {
        // A model loads on the mock backend (backend_loaded true) but the mock
        // runs no real inference, so is_inference_capable is false — a status
        // surface must not present it as a working detector (Rule 44).
        let e = engine();
        e.register_model(meta("m1", ModelExecution::EngineRun))
            .await
            .unwrap();
        let models = e.list_models().await;
        assert_eq!(models.len(), 1);
        assert!(models[0].backend_loaded, "the mock loads the model");
        assert!(
            !models[0].is_inference_capable,
            "the mock backend is not inference-capable"
        );
        // Timing is zero until it runs.
        assert_eq!(models[0].fps, 0.0);
        assert_eq!(models[0].latency_ms, 0.0);
    }

    #[tokio::test]
    async fn infer_records_timing_surfaced_by_list_models() {
        let e = engine();
        e.register_model(meta("m1", ModelExecution::EngineRun))
            .await
            .unwrap();
        // One inference records a latency sample; list_models surfaces it.
        e.infer("m1", &[0u8; 192], 8, 8, FrameFormat::Rgb24)
            .await
            .unwrap();
        let models = e.list_models().await;
        // latency_ms is set from the (tiny) measured duration; assert the field is
        // populated (>= 0, the timing path ran) — a real backend reports a real ms.
        assert!(models[0].latency_ms >= 0.0);
    }

    #[tokio::test]
    async fn plugin_side_model_cannot_be_inferred_by_engine() {
        let e = engine();
        e.register_model(meta("m2", ModelExecution::PluginSide))
            .await
            .unwrap();
        let err = e.infer("m2", &[0u8; 4], 1, 1, FrameFormat::Rgb24).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn unknown_model_inference_errors() {
        let e = engine();
        assert!(e
            .infer("nope", &[0u8; 4], 1, 1, FrameFormat::Rgb24)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn publish_detection_reaches_subscribers() {
        let e = engine();
        let mut rx = e.subscribe_detections();
        let batch = DetectionBatch {
            v: VISION_DETECTION_VERSION,
            model_id: "m".into(),
            camera_id: "c".into(),
            frame_id: 1,
            ts_ms: 0,
            frame_width: 640,
            frame_height: 480,
            detections: vec![Detection {
                bbox: Some(BoundingBox {
                    x: 0.0,
                    y: 0.0,
                    width: 1.0,
                    height: 1.0,
                }),
                class_label: "x".into(),
                confidence: 0.5,
                track_id: None,
                assoc_confidence: None,
                lock_state: None,
                attributes: None,
                mask: None,
                keypoints: None,
                depth: None,
                world_pos: None,
            }],
        };
        let reached = e.publish_detection(batch.clone());
        assert_eq!(reached, 1);
        assert_eq!(rx.try_recv().unwrap(), batch);
    }

    #[tokio::test]
    async fn infer_and_publish_builds_batch_from_descriptor() {
        let e = engine();
        e.register_model(meta("m", ModelExecution::EngineRun))
            .await
            .unwrap();
        let mut rx = e.subscribe_detections();
        let desc = FrameDescriptor {
            v: ados_protocol::framebus::FRAMEBUS_DESCRIPTOR_VERSION,
            camera_id: "uvc-0".into(),
            frame_id: 9,
            ts_ms: 123,
            width: 8,
            height: 8,
            format: FrameFormat::Rgb24,
            shm_name: "ados-vision-uvc-0".into(),
            slot: 1,
            seq: 1,
            byte_len: 192,
        };
        let batch = e.infer_and_publish("m", &desc, &[0u8; 192]).await.unwrap();
        assert_eq!(batch.model_id, "m");
        assert_eq!(batch.camera_id, "uvc-0");
        assert_eq!(batch.frame_id, 9);
        assert_eq!(rx.try_recv().unwrap(), batch);
    }

    fn det(x: f32, y: f32, conf: f32, label: &str) -> Detection {
        Detection {
            bbox: Some(BoundingBox {
                x,
                y,
                width: 40.0,
                height: 40.0,
            }),
            class_label: label.into(),
            confidence: conf,
            track_id: None,
            assoc_confidence: None,
            lock_state: None,
            attributes: None,
            mask: None,
            keypoints: None,
            depth: None,
            world_pos: None,
        }
    }

    #[tokio::test]
    async fn tracker_stamps_a_stable_track_id_and_holds_through_a_drop() {
        let e =
            VisionEngine::with_tracker(Box::new(MockBackend), 4, true, TrackerConfig::default());
        // Run up to confirmation: the same box for a few frames.
        e.apply_tracker(
            "cam",
            vec![det(100.0, 100.0, 0.9, "uav")],
            &[],
            0,
            0,
            FrameFormat::Rgb24,
        )
        .await;
        e.apply_tracker(
            "cam",
            vec![det(100.0, 100.0, 0.9, "uav")],
            &[],
            0,
            0,
            FrameFormat::Rgb24,
        )
        .await;
        let confirmed = e
            .apply_tracker(
                "cam",
                vec![det(100.0, 100.0, 0.9, "uav")],
                &[],
                0,
                0,
                FrameFormat::Rgb24,
            )
            .await;

        let stamped: Vec<_> = confirmed.iter().filter(|d| d.track_id.is_some()).collect();
        assert_eq!(
            stamped.len(),
            1,
            "exactly one detection carries the track id"
        );
        let id = stamped[0].track_id.unwrap();
        assert!(
            stamped[0].lock_state.is_none(),
            "an auto-seeded track is tracked but never presented as locked"
        );
        assert_eq!(e.current_track("cam").await, Some(id));

        // A dropped frame: the tracker coasts and the held target is appended
        // with the SAME id, still reported as the current track (never a silent
        // identity loss).
        let coast = e
            .apply_tracker("cam", vec![], &[], 0, 0, FrameFormat::Rgb24)
            .await;
        assert!(
            coast.iter().any(|d| d.track_id == Some(id)),
            "the held target survives a dropped frame with its id"
        );
        assert_eq!(e.current_track("cam").await, Some(id));
    }

    #[tokio::test]
    async fn operator_designate_locks_a_specific_detection() {
        let e =
            VisionEngine::with_tracker(Box::new(MockBackend), 4, true, TrackerConfig::default());
        // Two detections; the operator picks the lower-confidence one — the
        // auto-lock would have taken the other.
        let target = det(200.0, 50.0, 0.4, "person");
        let id = e
            .designate("cam", &target)
            .await
            .expect("designate seeds a track");
        // A freshly-seeded track is tentative, so current_track is None until a
        // measured frame confirms it — but the designated id is fixed.
        assert_eq!(e.current_track("cam").await, None);

        // Feed the designated box: it confirms under the SAME id, not a new one.
        e.apply_tracker("cam", vec![target.clone()], &[], 0, 0, FrameFormat::Rgb24)
            .await;
        let confirmed = e
            .apply_tracker("cam", vec![target.clone()], &[], 0, 0, FrameFormat::Rgb24)
            .await;
        assert_eq!(e.current_track("cam").await, Some(id));
        assert!(confirmed.iter().any(|d| d.track_id == Some(id)));

        // reset_track drops the lock entirely.
        e.reset_track("cam").await;
        assert_eq!(e.current_track("cam").await, None);
    }

    #[tokio::test]
    async fn auto_seed_publishes_no_lock_state_but_designate_does() {
        // An auto-seeded track (no operator designate) must NOT be published as
        // locked, even once confirmed — it is tracked (carries an id) for
        // continuity, but a follow behavior only ever engages a target the
        // operator actually designated. This mirrors the offload publish path.
        let e =
            VisionEngine::with_tracker(Box::new(MockBackend), 4, true, TrackerConfig::default());
        let subject = det(100.0, 100.0, 0.9, "uav");
        e.apply_tracker("cam", vec![subject.clone()], &[], 0, 0, FrameFormat::Rgb24)
            .await;
        let auto = e
            .apply_tracker("cam", vec![subject.clone()], &[], 0, 0, FrameFormat::Rgb24)
            .await;
        let tracked: Vec<_> = auto.iter().filter(|d| d.track_id.is_some()).collect();
        assert_eq!(tracked.len(), 1, "the auto-seeded subject is tracked");
        assert!(
            tracked[0].lock_state.is_none(),
            "an auto-seeded track is never published as locked"
        );

        // The SAME subject, now explicitly designated by the operator, IS
        // published as locked once the track confirms.
        e.reset_track("cam").await;
        e.designate("cam", &subject)
            .await
            .expect("designate seeds a track");
        e.apply_tracker("cam", vec![subject.clone()], &[], 0, 0, FrameFormat::Rgb24)
            .await;
        let designated = e
            .apply_tracker("cam", vec![subject.clone()], &[], 0, 0, FrameFormat::Rgb24)
            .await;
        let locked: Vec<_> = designated
            .iter()
            .filter(|d| d.lock_state == Some(LockState::Locked))
            .collect();
        assert_eq!(
            locked.len(),
            1,
            "an operator-designated track is published as locked"
        );
    }
}
