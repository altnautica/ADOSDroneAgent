//! Where a camera's detector batches spend their time: one batch's stage
//! times, and the running means `vision_status` reports per camera.

use ados_protocol::vision_status::DetectorBatchStatus;

/// Where one batch spent its time, in milliseconds.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct BatchTimes {
    /// Capture to the start of the batch: the frame's wait for the detector.
    pub(super) frame_age_ms: f32,
    /// The whole-frame pass, when it ran.
    pub(super) full_frame_ms: Option<f32>,
    /// The crop pass, when it ran.
    pub(super) crop_ms: Option<f32>,
    /// Within the passes, as the backend measured them (summed over the
    /// batch's passes).
    pub(super) stages: Option<crate::backend::StageTimes>,
    /// Re-id embeddings and the tracker.
    pub(super) track_ms: f32,
    /// Capture to publish.
    pub(super) capture_to_publish_ms: f32,
}

impl BatchTimes {
    /// Add a pass's backend stage times.
    pub(super) fn add_stages(&mut self, s: Option<crate::backend::StageTimes>) {
        let Some(s) = s else { return };
        let t = self.stages.get_or_insert_with(Default::default);
        t.preprocess_ms += s.preprocess_ms;
        t.run_ms += s.run_ms;
        t.decode_ms += s.decode_ms;
    }
}

/// An exponentially weighted mean, empty until its first sample.
#[derive(Debug, Clone, Copy, Default)]
struct Ewma(Option<f32>);

impl Ewma {
    const ALPHA: f32 = 0.2;

    fn add(&mut self, v: f32) {
        self.0 = Some(match self.0 {
            None => v,
            Some(m) => Self::ALPHA * v + (1.0 - Self::ALPHA) * m,
        });
    }

    fn us(&self) -> u64 {
        self.0.map_or(0, |ms| (ms.max(0.0) * 1000.0).round() as u64)
    }
}

/// One camera's batch counts and stage-time means.
#[derive(Debug, Clone, Default)]
pub(super) struct BatchStats {
    batches: u64,
    full_frame_runs: u64,
    frame_age: Ewma,
    full_frame: Ewma,
    crop: Ewma,
    preprocess: Ewma,
    inference: Ewma,
    decode: Ewma,
    track: Ewma,
    capture_to_publish: Ewma,
}

impl BatchStats {
    /// Count a published batch and fold in its times.
    pub(super) fn note(&mut self, times: &BatchTimes) {
        self.batches += 1;
        self.frame_age.add(times.frame_age_ms);
        if let Some(ms) = times.full_frame_ms {
            self.full_frame_runs += 1;
            self.full_frame.add(ms);
        }
        if let Some(ms) = times.crop_ms {
            self.crop.add(ms);
        }
        if let Some(s) = times.stages {
            self.preprocess.add(s.preprocess_ms);
            self.inference.add(s.run_ms);
            self.decode.add(s.decode_ms);
        }
        self.track.add(times.track_ms);
        self.capture_to_publish.add(times.capture_to_publish_ms);
    }

    /// The counts and means, in microseconds.
    pub(super) fn status(&self) -> DetectorBatchStatus {
        DetectorBatchStatus {
            batches: self.batches,
            full_frame_runs: self.full_frame_runs,
            frame_age_us: self.frame_age.us(),
            full_frame_us: self.full_frame.us(),
            crop_us: self.crop.us(),
            preprocess_us: self.preprocess.us(),
            inference_us: self.inference.us(),
            decode_us: self.decode.us(),
            track_us: self.track.us(),
            capture_to_publish_us: self.capture_to_publish.us(),
        }
    }
}
