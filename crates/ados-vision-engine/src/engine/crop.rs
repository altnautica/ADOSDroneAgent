//! The detector pass on a crop around the designated track, and the batch
//! schedule that makes it the primary pass while a subject is followed.
//!
//! A detector run on the whole frame squeezes the frame into its input, and a
//! person that is small in the frame falls below the size the model finds
//! reliably. A follow subject walking away crosses that size within seconds of
//! the designation, so the lock coasts and is lost while the subject is still
//! in plain view. While an operator-designated track is live, the engine runs
//! the detector on a window around the track's predicted box, sized so the
//! subject lands near `CROP_SUBJECT_MODEL_HEIGHT` in the model's input, and
//! feeds what it finds, in frame coordinates, to the tracker.
//!
//! The crop is the primary pass: every batch runs it, and the whole frame runs
//! only every `full_frame_interval` (the detector's `full_frame_hz`), to find a
//! subject that left the window and to see the people around it. A batch that
//! runs the whole frame runs the crop too only when the window magnifies the
//! subject beyond the whole frame's own scale. The crop pass may run its own
//! model (`crop_model_path`, the detector at a smaller input): the window
//! carries the magnification, so a small input holds a designated subject as
//! well as a large one at a fraction of the cost. Without a designated track
//! every batch is a whole-frame pass. All passes run through the same
//! accelerator lease and priority, so the detector's priority over depth is
//! unchanged.
//!
//! A crop that keeps missing a coasting subject may be looking in the wrong
//! place (the subject stepped out of the window): after
//! `CROP_MISSES_TO_WHOLE` crop-only batches in a row that leave the track
//! coasting, the next batch runs the whole frame too, and the count starts
//! again. While the subject stays lost the whole frame runs on every third
//! batch, not every batch: a subject small enough to need the crop is below
//! the whole frame's size floor, and a whole pass costs several crops.

use std::collections::HashMap;

use ados_protocol::framebus::BoundingBox;
use ados_protocol::vision_status::DetectorBatchStatus;

use super::batch_times::{BatchStats, BatchTimes};
use super::*;

pub(super) mod window;
pub(super) use window::{crop_to_frame, crop_window, merge_crop};

/// How early a whole-frame pass counts as due: half a 30 fps frame, as the
/// detector's own pace (`perception_scheduler`).
const FULL_FRAME_SLACK_NS: u64 = 16_000_000;

/// Crop-only batches in a row after which the track still coasts, before the
/// next batch runs the whole frame as well.
pub(super) const CROP_MISSES_TO_WHOLE: u32 = 2;

/// What one detector batch runs.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct BatchPlan {
    /// The whole-frame pass.
    pub(super) whole: bool,
    /// The crop pass around the designated track.
    pub(super) crop: Option<CropPass>,
    /// While a designated track is live: the window a crop pass around it
    /// looks in (unmagnified) and its class, which a whole-frame pass's hit
    /// is counted in.
    pub(super) subject: Option<(BoundingBox, String)>,
}

/// One crop pass: the model, the frame window and its input size, and the
/// designated track's class.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct CropPass {
    pub(super) model_id: String,
    pub(super) window: BoundingBox,
    pub(super) input: (u32, u32),
    pub(super) class: String,
}

/// The crop-primary schedule of one detector.
#[derive(Debug, Clone, Default)]
struct CropSetup {
    /// The model the crop pass runs, when not the detector.
    crop_model_id: Option<String>,
    /// The whole-frame pass's interval while a designated subject is tracked;
    /// `0` runs it every batch.
    full_frame_interval_ns: u64,
}

/// One camera's detector batches: the last whole-frame pass, the crop
/// counters, the crop misses in a row, a pending designation and the stage
/// times.
#[derive(Debug, Clone, Default)]
struct CameraBatches {
    last_full_ts_ns: Option<u64>,
    counts: CropCounts,
    /// Crop batches in a row after which the designated track still coasted.
    crop_misses: u32,
    designated: bool,
    stats: BatchStats,
}

/// The crop-primary schedules and every camera's batch state.
#[derive(Debug, Default)]
pub(super) struct BatchState {
    setups: HashMap<String, CropSetup>,
    cameras: HashMap<String, CameraBatches>,
}

impl VisionEngine {
    fn batch_state(&self) -> std::sync::MutexGuard<'_, BatchState> {
        self.batches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Set detector `detector_id`'s crop-primary schedule: the model its crop
    /// pass runs (`None`: the detector itself) and, while a designated subject
    /// is tracked, the whole-frame pass's interval (`0`: every batch).
    pub fn configure_crop_primary(
        &self,
        detector_id: &str,
        crop_model_id: Option<String>,
        full_frame_interval_ns: u64,
    ) {
        self.batch_state().setups.insert(
            detector_id.to_string(),
            CropSetup {
                crop_model_id,
                full_frame_interval_ns,
            },
        );
    }

    /// Whether a designation landed on `camera_id` since the last call. The
    /// capture loop runs a batch on the next frame when it did, so the
    /// designated subject is looked at at once rather than at the end of the
    /// detector's current interval.
    pub fn take_designation(&self, camera_id: &str) -> bool {
        self.batch_state()
            .cameras
            .get_mut(camera_id)
            .is_some_and(|c| std::mem::take(&mut c.designated))
    }

    /// Note a designation on `camera_id` for [`Self::take_designation`].
    pub(super) fn note_designation(&self, camera_id: &str) {
        let mut state = self.batch_state();
        let c = state.cameras.entry(camera_id.to_string()).or_default();
        c.designated = true;
        c.crop_misses = 0;
    }

    /// What detector `model_id`'s batch on the frame `desc` names runs: the
    /// whole frame alone when the camera holds no live designated track (or
    /// the frame is not rgb24, which the crop is cut from); otherwise the crop
    /// around the track's predicted box, joined by the whole frame when its
    /// interval is up or the coasting track's crop batches missed
    /// [`CROP_MISSES_TO_WHOLE`] times in a row (and then the crop only when it
    /// magnifies the subject).
    pub(super) async fn plan_batch(&self, model_id: &str, desc: &FrameDescriptor) -> BatchPlan {
        let whole_only = BatchPlan {
            whole: true,
            crop: None,
            subject: None,
        };
        if !self.tracker_enabled || desc.format != FrameFormat::Rgb24 {
            return whole_only;
        }
        // The tracker steps on the frame clock (a simulated frame's sim time,
        // else its capture time): the window is predicted on the same one.
        let frame_ts = crate::sim_pacing::frame_clock_ns(desc).unwrap_or(desc.ts_ns);
        let Some((predicted, class, coast_steps)) = self
            .trackers
            .lock()
            .await
            .get(&desc.camera_id)
            .and_then(|t| {
                t.designated_prediction_at(frame_ts)
                    .map(|(b, c)| (b, c.to_string(), t.coast_steps()))
            })
        else {
            return whole_only;
        };
        let (crop_id, interval_ns, last_full_ns, crop_misses) = {
            let state = self.batch_state();
            let setup = state.setups.get(model_id).cloned().unwrap_or_default();
            let cam = state.cameras.get(&desc.camera_id);
            (
                setup.crop_model_id,
                setup.full_frame_interval_ns,
                cam.and_then(|c| c.last_full_ts_ns),
                cam.map_or(0, |c| c.crop_misses),
            )
        };
        let (crop_model, crop_input, whole_input_h) = {
            let models = self.models.lock().await;
            let dims = |id: &str| {
                models
                    .get(id)
                    .filter(|m| m.loaded.is_some())
                    .map(|m| (m.meta.input_width, m.meta.input_height))
            };
            let Some(whole) = dims(model_id) else {
                return whole_only;
            };
            // A crop model that did not load leaves the crop to the detector.
            match crop_id.and_then(|c| dims(&c).map(|d| (c, d))) {
                Some((c, d)) => (c, d, whole.1),
                None => (model_id.to_string(), whole, whole.1),
            }
        };
        let interval_due = interval_ns == 0
            || last_full_ns
                .is_none_or(|t| desc.ts_ns.saturating_sub(t) + FULL_FRAME_SLACK_NS >= interval_ns);
        let crop_lost = coast_steps > 0.0 && crop_misses >= CROP_MISSES_TO_WHOLE;
        let whole_due = interval_due || crop_lost;
        let magnify_over = whole_due.then(|| whole_input_h as f32 / desc.height as f32);
        let window = crop_window(
            &predicted,
            coast_steps,
            desc.width,
            desc.height,
            crop_input.0,
            crop_input.1,
            magnify_over,
        );
        let looked_in = crop_window(
            &predicted,
            coast_steps,
            desc.width,
            desc.height,
            crop_input.0,
            crop_input.1,
            None,
        );
        BatchPlan {
            whole: whole_due || window.is_none(),
            crop: window.map(|window| CropPass {
                model_id: crop_model,
                window,
                input: crop_input,
                class: class.clone(),
            }),
            subject: looked_in.map(|w| (w, class)),
        }
    }

    /// After the batch's tracker step, while a designated track is live: a
    /// crop-only batch after which the track still coasts is a crop miss; an
    /// association, or a batch that ran the whole frame (it just looked
    /// outside the window), ends the run of misses. So while the subject stays
    /// lost the whole frame runs on at most every third batch: on a small
    /// subject it finds nothing the crop does not (the subject sits below its
    /// size floor) and costs several crops, so running it every batch halved
    /// the crop rate and kept the lock from confirming.
    pub(super) fn note_crop_outcome(
        &self,
        camera_id: &str,
        crop_ran: bool,
        whole_ran: bool,
        associated: bool,
    ) {
        let mut state = self.batch_state();
        let c = state.cameras.entry(camera_id.to_string()).or_default();
        if associated || whole_ran {
            c.crop_misses = 0;
        } else if crop_ran {
            c.crop_misses += 1;
        }
    }

    /// Count a whole-frame pass that ran while a designated track is live,
    /// and a hit when it found a box of the track's class whose centre lies in
    /// the window `subject` a crop around it looks in.
    pub(super) fn note_whole_pass(
        &self,
        camera_id: &str,
        subject: &(BoundingBox, String),
        found: &[Detection],
    ) {
        let (w, class) = subject;
        let hit = found.iter().any(|d| {
            d.class_label == *class
                && d.bbox.as_ref().is_some_and(|b| {
                    let (cx, cy) = (b.x + b.width / 2.0, b.y + b.height / 2.0);
                    cx >= w.x && cx <= w.x + w.width && cy >= w.y && cy <= w.y + w.height
                })
        });
        let mut state = self.batch_state();
        let c = &mut state
            .cameras
            .entry(camera_id.to_string())
            .or_default()
            .counts;
        c.whole_runs += 1;
        c.whole_hits += u64::from(hit);
    }

    /// Run `pass` on the frame `desc` names and return the subject-class
    /// detections in frame pixels, with the pass's time and backend stages. A
    /// crop model of its own records its timing under its own id; the
    /// detector's crop is part of the detector's batch timing.
    pub(super) async fn run_crop_pass(
        &self,
        detector_id: &str,
        desc: &FrameDescriptor,
        frame: &[u8],
        pass: &CropPass,
    ) -> Result<(Vec<Detection>, f32, Option<crate::backend::StageTimes>)> {
        let p = self
            .run_pass(
                &pass.model_id,
                frame,
                desc.width,
                desc.height,
                desc.format,
                Some(&pass.window),
            )
            .await?;
        if pass.model_id != detector_id && p.result.is_ok() {
            let now_ms = (ados_protocol::framebus::mono_ns() / 1_000_000) as i64;
            self.record_timing(&p.key, p.latency_ms, now_ms).await;
        }
        let (iw, ih) = pass.input;
        let found = crop_to_frame(
            p.result?,
            &pass.window,
            desc.width,
            desc.height,
            iw,
            ih,
            &pass.class,
        );
        let mut state = self.batch_state();
        let c = &mut state
            .cameras
            .entry(desc.camera_id.clone())
            .or_default()
            .counts;
        c.runs += 1;
        c.hits += u64::from(!found.is_empty());
        Ok((found, p.latency_ms, p.stages))
    }

    /// Record a published batch on the frame `desc` names: its stage times,
    /// and the whole-frame pass's time when it ran.
    pub(super) fn note_batch(&self, desc: &FrameDescriptor, times: &BatchTimes) {
        let mut state = self.batch_state();
        let c = state.cameras.entry(desc.camera_id.clone()).or_default();
        if times.full_frame_ms.is_some() {
            c.last_full_ts_ns = Some(desc.ts_ns);
        }
        c.stats.note(times);
    }

    /// The crop passes `camera_id` has run and found its subject on.
    pub(super) fn crop_counts(&self, camera_id: &str) -> CropCounts {
        self.batch_state()
            .cameras
            .get(camera_id)
            .map(|c| c.counts)
            .unwrap_or_default()
    }

    /// Where `camera_id`'s detector batches spend their time.
    pub(super) fn batch_status(&self, camera_id: &str) -> DetectorBatchStatus {
        self.batch_state()
            .cameras
            .get(camera_id)
            .map(|c| c.stats.status())
            .unwrap_or_default()
    }
}

/// One camera's per-pass counters while a designated track is live.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct CropCounts {
    /// Batches the crop pass ran on.
    pub(super) runs: u64,
    /// Of those, batches it found a box of the designated track's class on.
    pub(super) hits: u64,
    /// Batches the whole-frame pass ran on.
    pub(super) whole_runs: u64,
    /// Of those, batches it found a box of the track's class centred in the
    /// window a crop around the track looks in.
    pub(super) whole_hits: u64,
}
