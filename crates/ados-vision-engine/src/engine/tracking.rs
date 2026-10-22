//! Detection publishing and the per-camera single-object tracker.

use super::*;

impl VisionEngine {
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
            ts_ms: (desc.ts_ns / 1_000_000) as i64,
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
    pub(super) async fn apply_tracker(
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
    pub(super) async fn extract_appearances(
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
                d.bbox.as_ref().and_then(|b| {
                    ados_vision::reid::crop_resize_rgb24(frame, width, height, b, iw, ih)
                })
            })
            .collect();

        // Embed under the accelerator lease, with the registry lock released
        // before the blocking calls. If the lease can't be acquired (a closing
        // engine), degrade to motion-only rather than running embeds lease-less
        // against a concurrent detector inference.
        let _permit = match self.accel_lease.acquire().await {
            Ok(p) => p,
            Err(_) => return none(),
        };
        let loaded = {
            let models = self.models.lock().await;
            match models.get(&model_id).and_then(|reg| reg.loaded.clone()) {
                Some(loaded) => loaded,
                None => return none(),
            }
        };
        run_blocking(|| {
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
        })
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
    update: ados_vision::tracker::TrackUpdate,
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
