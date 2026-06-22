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
};
use anyhow::{anyhow, Result};
use tokio::sync::{broadcast, Mutex, Semaphore};

use crate::backend::{LoadedModel, VisionBackend};
use crate::ring::RingWriter;

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
}

impl VisionEngine {
    /// Build the engine around a chosen backend.
    pub fn new(backend: Box<dyn VisionBackend>, slot_count: u32) -> Arc<Self> {
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
        })
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
        loaded.infer(frame, width, height, format)
    }

    /// Publish a detection batch on `vision.detection`. A plugin-side model
    /// calls this; an engine-run flow calls it after [`Self::infer`]. Returns
    /// the subscriber count the batch reached (0 when none).
    pub fn publish_detection(&self, batch: DetectionBatch) -> usize {
        self.detection_tx.send(batch).unwrap_or(0)
    }

    /// Convenience for the engine-run flow: infer then publish, building the
    /// batch from the frame descriptor and the model's id.
    pub async fn infer_and_publish(
        &self,
        model_id: &str,
        desc: &FrameDescriptor,
        frame: &[u8],
    ) -> Result<DetectionBatch> {
        let detections = self
            .infer(model_id, frame, desc.width, desc.height, desc.format)
            .await?;
        let batch = DetectionBatch {
            model_id: model_id.to_string(),
            camera_id: desc.camera_id.clone(),
            frame_id: desc.frame_id,
            ts_ms: desc.ts_ms,
            detections,
        };
        self.publish_detection(batch.clone());
        Ok(batch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MockBackend;
    use ados_protocol::framebus::{BoundingBox, ModelKind};

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
            model_id: "m".into(),
            camera_id: "c".into(),
            frame_id: 1,
            ts_ms: 0,
            detections: vec![Detection {
                bbox: BoundingBox {
                    x: 0.0,
                    y: 0.0,
                    width: 1.0,
                    height: 1.0,
                },
                class_label: "x".into(),
                confidence: 0.5,
                track_id: None,
                assoc_confidence: None,
                lock_state: None,
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
}
