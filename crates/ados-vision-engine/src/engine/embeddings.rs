//! Per-detection appearance embeddings.
//!
//! One extraction per frame feeds two consumers:
//!
//! - the tracker, when `reid_enabled`, associates on them (re-identification
//!   across a crossing or a brief occlusion);
//! - subscribers that asked for them (`vision.subscribe_detections` with
//!   `embeddings: true`) get them on each detection as the attributes
//!   [`EMBEDDING_ATTR`] (an L2-normalized float list) and
//!   [`EMBEDDING_MODEL_ATTR`] (what produced it). Nothing is extracted for the
//!   wire while no subscriber asks, and the push path strips them for
//!   connections that did not ask.
//!
//! The source is the registered re-id model (`vision.reid`) when it is loaded,
//! else the model-free colour histogram ([`HISTOGRAM_EMBEDDING`]). One frame
//! never mixes the two: embeddings from different sources have different
//! lengths and are not comparable, so a consumer compares only embeddings with
//! the same [`EMBEDDING_MODEL_ATTR`].

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use super::*;

/// Detection attribute carrying the L2-normalized embedding (a float list).
pub const EMBEDDING_ATTR: &str = "embedding";
/// Detection attribute naming the embedding source: the re-id model id, or
/// [`HISTOGRAM_EMBEDDING`].
pub const EMBEDDING_MODEL_ATTR: &str = "embedding_model";
/// The embedding source name of the model-free colour histogram.
pub const HISTOGRAM_EMBEDDING: &str = "histogram";

/// One frame's embeddings, in detection order, and their source.
pub(super) struct FrameAppearances {
    pub per_detection: Vec<Option<Appearance>>,
    pub source: String,
}

/// A registered request for embeddings on the detection wire. The engine
/// extracts embeddings while at least one is held; dropping it withdraws the
/// request.
pub struct EmbeddingInterest {
    engine: Arc<VisionEngine>,
}

impl Drop for EmbeddingInterest {
    fn drop(&mut self) {
        self.engine
            .embedding_consumers
            .fetch_sub(1, Ordering::AcqRel);
    }
}

impl VisionEngine {
    /// Register a consumer of wire embeddings (one `vision.sock` connection
    /// that subscribed with `embeddings: true`).
    pub fn request_embeddings(self: &Arc<Self>) -> EmbeddingInterest {
        self.embedding_consumers.fetch_add(1, Ordering::AcqRel);
        EmbeddingInterest {
            engine: self.clone(),
        }
    }

    /// Whether any subscriber currently wants embeddings on the wire.
    pub fn embeddings_requested(&self) -> bool {
        self.embedding_consumers.load(Ordering::Acquire) > 0
    }

    /// Extract a per-detection embedding: crop each box to the re-id model's
    /// input, run its `embed`, and L2-normalize; or, when no re-id model is
    /// loaded, compute the colour histogram descriptor. A `None` entry means no
    /// embedding for that box (box-less, degenerate crop, or a failed embed),
    /// which the tracker treats as motion-only. A non-rgb24 frame yields no
    /// embeddings rather than a guessed conversion. Model embeds run under the
    /// accelerator lease so they never contend with the detector on the NPU.
    pub(super) async fn extract_appearances(
        &self,
        frame: &[u8],
        width: u32,
        height: u32,
        format: FrameFormat,
        detections: &[Detection],
    ) -> FrameAppearances {
        let none = |source: &str| FrameAppearances {
            per_detection: vec![None; detections.len()],
            source: source.to_string(),
        };
        if format != FrameFormat::Rgb24 {
            return none(HISTOGRAM_EMBEDDING);
        }
        // The re-id model's input dims, if it is registered + loaded.
        let model = match self.reid_model_id.clone() {
            Some(id) => {
                let models = self.models.lock().await;
                match models.get(&id) {
                    Some(reg) if reg.loaded.is_some() => {
                        Some((id, reg.meta.input_width, reg.meta.input_height))
                    }
                    _ => None,
                }
            }
            None => None,
        };
        let Some((model_id, iw, ih)) = model else {
            return FrameAppearances {
                per_detection: detections
                    .iter()
                    .map(|d| {
                        d.bbox.as_ref().and_then(|b| {
                            ados_vision::histogram::histogram_descriptor(frame, width, height, b)
                                .map(Appearance::from_features)
                        })
                    })
                    .collect(),
                source: HISTOGRAM_EMBEDDING.to_string(),
            };
        };
        // Crop every box first (no lock needed). A box-less percept has no box
        // to crop, so it gets no embedding.
        let crops: Vec<Option<Vec<u8>>> = detections
            .iter()
            .map(|d| {
                d.bbox.as_ref().and_then(|b| {
                    ados_vision::reid::crop_resize_rgb24(frame, width, height, b, iw, ih)
                })
            })
            .collect();

        // Embed under the accelerator lease, with the registry lock released
        // before the blocking calls. If the lease can't be acquired (a closing
        // engine), yield no embeddings rather than running embeds lease-less
        // against a concurrent detector inference.
        let _permit = {
            let _waiting = super::depth::PriorityWaiter::new(&self.priority_waiters);
            match self.accel_lease.acquire().await {
                Ok(p) => p,
                Err(_) => return none(&model_id),
            }
        };
        let loaded = {
            let models = self.models.lock().await;
            match models.get(&model_id).and_then(|reg| reg.loaded.clone()) {
                Some(loaded) => loaded,
                None => return none(&model_id),
            }
        };
        let per_detection = run_blocking(|| {
            crops
                .into_iter()
                .map(|crop| {
                    let crop = crop?;
                    match loaded.embed(&crop, iw, ih, FrameFormat::Rgb24) {
                        Ok(Some(mut emb)) if !emb.is_empty() => {
                            ados_vision::reid::l2_normalize(&mut emb);
                            Some(Appearance::from_features(emb))
                        }
                        _ => None,
                    }
                })
                .collect()
        });
        FrameAppearances {
            per_detection,
            source: model_id,
        }
    }
}

/// Put each detection's embedding and its source on the detection's
/// attributes. Detections without an embedding are left unchanged.
pub(super) fn stamp_embeddings(detections: &mut [Detection], apps: &FrameAppearances) {
    for (det, app) in detections.iter_mut().zip(&apps.per_detection) {
        let Some(app) = app else { continue };
        let attrs = det.attributes.get_or_insert_with(BTreeMap::new);
        attrs.insert(
            EMBEDDING_ATTR.to_string(),
            rmpv::Value::Array(app.features.iter().map(|&v| rmpv::Value::F32(v)).collect()),
        );
        attrs.insert(
            EMBEDDING_MODEL_ATTR.to_string(),
            rmpv::Value::from(apps.source.as_str()),
        );
    }
}

/// Remove the embedding attributes from every detection of `batch` (for a
/// subscriber that did not ask for them). An attribute map left empty is
/// dropped so the detection reads exactly as it did before embeddings existed.
/// Returns whether anything was removed.
pub fn strip_embeddings(batch: &mut DetectionBatch) -> bool {
    let mut removed = false;
    for det in &mut batch.detections {
        let Some(attrs) = det.attributes.as_mut() else {
            continue;
        };
        removed |= attrs.remove(EMBEDDING_ATTR).is_some();
        removed |= attrs.remove(EMBEDDING_MODEL_ATTR).is_some();
        if attrs.is_empty() {
            det.attributes = None;
        }
    }
    removed
}
