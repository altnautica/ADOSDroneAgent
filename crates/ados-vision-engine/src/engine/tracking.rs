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
    /// operator-designated track, the batch runs the crop pass around the
    /// track's predicted box, and the whole frame only when its interval is up
    /// (see [`super::crop`]); otherwise it runs the whole frame. Boxes larger
    /// than the policy's `max_box_area_frac` of the frame are dropped first,
    /// whatever the backend. When the tracker is enabled, the detections pass
    /// through the camera's single-object tracker, advanced by the frame's
    /// capture time, so the locked object carries a stable `track_id` +
    /// `lock_state`. Embeddings are extracted once per frame, when the re-id
    /// tracker or a subscriber needs them, and put on the detections only for
    /// a subscriber. The batch's inference time is recorded as the detector's
    /// latency, and its stage times on the camera's status.
    pub async fn infer_and_publish(
        &self,
        model_id: &str,
        desc: &FrameDescriptor,
        frame: &[u8],
    ) -> Result<DetectionBatch> {
        let ms_since =
            |ts_ns: u64| ados_protocol::framebus::mono_ns().saturating_sub(ts_ns) as f32 / 1e6;
        let mut times = super::batch_times::BatchTimes {
            frame_age_ms: ms_since(desc.ts_ns),
            ..Default::default()
        };
        self.move_track_with_camera(desc).await;
        let plan = self.plan_batch(model_id, desc).await;
        let mut infer_ms = 0.0;
        let mut ran_whole = plan.whole;
        let mut detections = if plan.whole {
            self.run_whole_pass(model_id, desc, frame, &mut times, &mut infer_ms)
                .await?
        } else {
            Vec::new()
        };
        if let Some(crop) = &plan.crop {
            match self.run_crop_pass(model_id, desc, frame, crop).await {
                Ok((found, ms, stages)) => {
                    times.crop_ms = Some(ms);
                    times.add_stages(stages);
                    // The detector's latency is the batch's inference time,
                    // whichever model ran the crop.
                    infer_ms += ms;
                    super::crop::merge_crop(&mut detections, found);
                }
                Err(e) => {
                    tracing::warn!(model = %crop.model_id, camera = %desc.camera_id, error = %e, "designated_crop_infer_failed");
                    // A failed crop pass costs only the crop: whatever the
                    // whole frame found still publishes, and a crop-only
                    // batch runs the whole frame instead, so a failing crop
                    // never reads as an empty window to the tracker or an
                    // overlay.
                    if !ran_whole {
                        detections = self
                            .run_whole_pass(model_id, desc, frame, &mut times, &mut infer_ms)
                            .await?;
                        ran_whole = true;
                    }
                }
            }
        }
        super::demand::drop_oversized_boxes(
            &mut detections,
            desc.width,
            desc.height,
            self.policy.max_box_area_frac,
        );
        let track_start = std::time::Instant::now();
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
            self.track(&desc.camera_id, detections, apps, Some(desc.ts_ns))
                .await
        } else {
            detections
        };
        times.track_ms = track_start.elapsed().as_secs_f32() * 1000.0;
        let mut batch = self.detection_batch(model_id, desc, detections);
        if !ran_whole {
            batch.crop_window = plan.crop.as_ref().map(|c| c.window);
        }
        self.publish_detection(batch.clone());
        times.capture_to_publish_ms = ms_since(desc.ts_ns);
        let now_ms = (ados_protocol::framebus::mono_ns() / 1_000_000) as i64;
        // Every published batch ran a pass that measured: a failed crop-only
        // pass fell back to the whole frame, and a failed whole frame returned.
        self.record_timing(model_id, infer_ms, now_ms).await;
        self.note_batch(desc, &times);
        Ok(batch)
    }

    /// The whole-frame pass of a batch: its detections, with its time on
    /// `times` and added to the batch's inference time `infer_ms`.
    async fn run_whole_pass(
        &self,
        model_id: &str,
        desc: &FrameDescriptor,
        frame: &[u8],
        times: &mut super::batch_times::BatchTimes,
        infer_ms: &mut f32,
    ) -> Result<Vec<Detection>> {
        let pass = self
            .run_pass(model_id, frame, desc.width, desc.height, desc.format, None)
            .await?;
        let dets = pass.result?;
        times.full_frame_ms = Some(pass.latency_ms);
        times.add_stages(pass.stages);
        *infer_ms += pass.latency_ms;
        Ok(dets)
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
        self.track(camera_id, detections, appearances, None).await
    }

    /// The tracker step proper, on embeddings already extracted (or motion-only
    /// with `None`), advanced to the frame's capture time `ts_ns` (one step
    /// when `None`). Extraction happens before the tracker lock is taken: it
    /// takes the accelerator lease + the model lock, and keeping the lock order
    /// accel→models, distinct from the tracker lock, avoids any nesting.
    async fn track(
        &self,
        camera_id: &str,
        detections: Vec<Detection>,
        appearances: Option<embeddings::FrameAppearances>,
        ts_ns: Option<u64>,
    ) -> Vec<Detection> {
        let candidates: Vec<Candidate> = match appearances {
            Some(apps) => detections
                .iter()
                .cloned()
                .zip(apps.per_detection)
                .map(|(det, app)| match app {
                    Some(a) => Candidate::with_appearance(det, a),
                    None => Candidate::motion_only(det),
                })
                .collect(),
            None => detections
                .iter()
                .cloned()
                .map(Candidate::motion_only)
                .collect(),
        };
        let mut trackers = self.trackers.lock().await;
        let tracker = trackers
            .entry(camera_id.to_string())
            .or_insert_with(|| SingleObjectTracker::new(self.tracker_cfg));
        let update = match ts_ns {
            Some(ts) => tracker.update_with_appearance_at(&candidates, ts),
            None => tracker.update_with_appearance(&candidates),
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
    /// designation hold), so the lock has detections to follow, and asks the
    /// capture loop for a batch on the next frame
    /// ([`Self::take_designation`]), so the first locked box follows the tap
    /// by one frame and one crop pass.
    pub async fn designate(&self, camera_id: &str, target: &Detection) -> Option<u64> {
        self.grant_designation(camera_id, ados_protocol::framebus::mono_ns() / 1_000_000);
        let id = {
            let mut trackers = self.trackers.lock().await;
            let tracker = trackers
                .entry(camera_id.to_string())
                .or_insert_with(|| SingleObjectTracker::new(self.tracker_cfg));
            tracker.designate(target)
        };
        if id.is_some() {
            self.note_designation(camera_id);
        }
        id
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
