//! Tuning for the multi-object tracker.

/// Tuning for [`super::MultiObjectTracker`]. Times are seconds, image
/// quantities are pixels or fractions of a box's height, so the defaults hold
/// at any detector rate and frame size.
#[derive(Debug, Clone, Copy)]
pub struct MotConfig {
    /// Detections at or above this score take part in the first association
    /// pass and may start a track.
    pub high_score: f32,
    /// Detections between this and [`Self::high_score`] only extend a track
    /// the first pass left unmatched (an object partly hidden scores low but
    /// is still the same object). Below it a detection is ignored.
    pub low_score: f32,
    /// A high-score detection matched to no track starts a tentative track only
    /// at or above this score.
    pub new_track_score: f32,
    /// Least IoU between a track's predicted box and a high-score detection
    /// for them to match in the first pass.
    pub match_iou: f32,
    /// Least IoU for the low-score second pass: stricter, as a low score is
    /// weaker evidence.
    pub low_match_iou: f32,
    /// Least IoU for a tentative track to take a detection.
    pub tentative_iou: f32,
    /// Least IoU between a track's last observed box and a detection for the
    /// recovery pass, which re-finds a track whose prediction drifted while it
    /// was unseen.
    pub recovery_iou: f32,
    /// Weight of the motion-direction consistency term in the first pass
    /// cost (`0` turns it off).
    pub momentum_weight: f32,
    /// Normalised centre distance (Mahalanobis, two degrees of freedom) under
    /// which a lost track may take a detection its predicted box no longer
    /// overlaps. `0` turns the fallback off.
    pub lost_gate: f32,
    /// Bound on that fallback in box heights: a long-lost track's position
    /// uncertainty grows without limit, and a newcomer far from where the
    /// track was must not inherit its id.
    pub lost_max_dist_h: f32,
    /// Largest height ratio, either way, between a track and a detection it
    /// takes outside the IoU gate.
    pub max_height_ratio: f32,
    /// Consecutive hits that confirm a tentative track (and give it an id).
    pub birth_hits: u32,
    /// Seconds a confirmed track survives unseen before it is removed.
    pub max_age_s: f32,
    /// Seconds one update stands for when the caller gives no timestamp.
    pub nominal_dt_s: f32,
    /// Most tracks held at once; a new track beyond it is not started.
    pub max_tracks: usize,
    /// Motion model: acceleration noise of the box centre, in box heights per
    /// second squared.
    pub accel_noise_h: f32,
    /// Measurement noise of a box's centre and height, as a fraction of its
    /// height.
    pub meas_noise_h: f32,
    /// Seconds the designated subject's appearance model is kept after its
    /// track was last seen, for re-identification.
    pub reid_window_s: f32,
    /// Least re-identification score (`0..1`) for a candidate track to take the
    /// designated subject's id.
    pub reid_min_score: f32,
    /// Least lead of the best re-identification score over the next one (a
    /// competing candidate, or a person known not to be the subject). Below it
    /// the tracker does not re-lock: no lock is safer than a wrong one.
    pub reid_min_margin: f32,
    /// Feature observations a candidate needs before it can be re-identified.
    pub reid_min_obs: u32,
    /// Learning rate of the subject's online classifier.
    pub reid_learning_rate: f32,
    /// L2 decay of the subject's online classifier per update.
    pub reid_l2: f32,
    /// Update rate of the subject's appearance template (exponential mean).
    pub reid_template_rate: f32,
}

impl Default for MotConfig {
    fn default() -> Self {
        Self {
            high_score: 0.5,
            low_score: 0.1,
            new_track_score: 0.6,
            match_iou: 0.2,
            low_match_iou: 0.4,
            tentative_iou: 0.3,
            recovery_iou: 0.3,
            momentum_weight: 0.2,
            lost_gate: 9.0,
            lost_max_dist_h: 0.75,
            max_height_ratio: 1.5,
            birth_hits: 3,
            max_age_s: 1.5,
            nominal_dt_s: 0.1,
            max_tracks: 64,
            accel_noise_h: 2.0,
            meas_noise_h: 0.05,
            reid_window_s: 30.0,
            reid_min_score: 0.7,
            reid_min_margin: 0.15,
            reid_min_obs: 3,
            reid_learning_rate: 0.5,
            reid_l2: 1e-3,
            reid_template_rate: 0.1,
        }
    }
}
