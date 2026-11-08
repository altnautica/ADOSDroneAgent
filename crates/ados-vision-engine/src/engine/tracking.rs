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
    /// batch from the frame descriptor and the model's id. First the camera's
    /// track is moved by the vehicle's turn since the previous batch's frame
    /// (see [`super::camera_motion`]), so everything below predicts from where
    /// the subject now appears. While the camera's tracker holds an
    /// operator-designated track, the model also runs on a crop around the
    /// track's predicted box (see [`super::crop`]) and those detections join
    /// the whole frame's, so a subject too small for the whole-frame pass
    /// stays detected. Boxes larger than the policy's `max_box_area_frac` of
    /// the frame are dropped first, whatever the backend. When the tracker is
    /// enabled, the detections pass through the camera's single-object
    /// tracker so the locked object carries a stable `track_id` +
    /// `lock_state`. Embeddings are extracted once per frame, when the re-id
    /// tracker or a subscriber needs them, and put on the detections only for
    /// a subscriber.
    pub async fn infer_and_publish(
        &self,
        model_id: &str,
        desc: &FrameDescriptor,
        frame: &[u8],
    ) -> Result<DetectionBatch> {
        self.move_track_with_camera(desc).await;
        let mut detections = self
            .infer(model_id, frame, desc.width, desc.height, desc.format)
            .await?;
        // A failed crop pass costs only the crop: the whole frame's detections
        // still publish.
        match self.designated_crop_detections(model_id, desc, frame).await {
            Ok(crop) => super::crop::merge_crop(&mut detections, crop),
            Err(e) => {
                tracing::warn!(model = model_id, camera = %desc.camera_id, error = %e, "designated_crop_infer_failed");
            }
        }
        super::demand::drop_oversized_boxes(
            &mut detections,
            desc.width,
            desc.height,
            self.policy.max_box_area_frac,
        );
        let for_tracker = self.tracker_enabled && self.reid_enabled;
        let for_wire = self.embeddings_requested();
        let appearances = if for_tracker || for_wire {
            Some(
                self.extract_appearances(frame, desc.width, desc.height, desc.format, &detections)
                    .await,
            )
        } else {
            None
        };
        if for_wire {
            if let Some(apps) = &appearances {
                embeddings::stamp_embeddings(&mut detections, apps);
            }
        }
        let detections = if self.tracker_enabled {
            let apps = if for_tracker { appearances } else { None };
            self.track(&desc.camera_id, detections, apps).await
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
    /// followable. With re-id on, the frame's embeddings are extracted first.
    /// The tests' entry to the tracker; `infer_and_publish` extracts once and
    /// calls [`Self::track`] itself.
    #[cfg(test)]
    pub(super) async fn apply_tracker(
        &self,
        camera_id: &str,
        detections: Vec<Detection>,
        frame: &[u8],
        width: u32,
        height: u32,
        format: FrameFormat,
    ) -> Vec<Detection> {
        let appearances = if self.reid_enabled {
            Some(
                self.extract_appearances(frame, width, height, format, &detections)
                    .await,
            )
        } else {
            None
        };
        self.track(camera_id, detections, appearances).await
    }

    /// The tracker step proper, on embeddings already extracted (or motion-only
    /// with `None`). Extraction happens before the tracker lock is taken: it
    /// takes the accelerator lease + the model lock, and keeping the lock order
    /// accel→models, distinct from the tracker lock, avoids any nesting.
    async fn track(
        &self,
        camera_id: &str,
        detections: Vec<Detection>,
        appearances: Option<embeddings::FrameAppearances>,
    ) -> Vec<Detection> {
        let mut trackers = self.trackers.lock().await;
        let tracker = trackers
            .entry(camera_id.to_string())
            .or_insert_with(|| SingleObjectTracker::new(self.tracker_cfg));
        let update = match appearances {
            Some(apps) => {
                let candidates: Vec<Candidate> = detections
                    .iter()
                    .cloned()
                    .zip(apps.per_detection)
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
    /// track id. Builds the camera's tracker if it does not exist yet. The
    /// designation also holds detection on for the camera (the policy's
    /// designation hold), so the lock has detections to follow.
    pub async fn designate(&self, camera_id: &str, target: &Detection) -> Option<u64> {
        self.grant_designation(camera_id, ados_protocol::framebus::mono_ns() / 1_000_000);
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
pub(super) fn bbox_iou(
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
