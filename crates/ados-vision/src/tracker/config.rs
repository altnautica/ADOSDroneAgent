//! Tracker tuning and per-frame output types.

use ados_protocol::framebus::Detection;

/// Tuning for the tracker. Defaults are chosen for a hand-held / airborne
/// camera at a few tens of frames per second; all are overridable.
#[derive(Debug, Clone, Copy)]
pub struct TrackerConfig {
    /// Minimum IoU between a candidate detection and the predicted box for the
    /// candidate to be eligible for the lock. The primary geometric gate.
    pub min_iou: f32,
    /// Fallback center-distance gate, as a multiple of the predicted box's
    /// diagonal. A fast/weaving target can move enough that its boxes barely
    /// overlap frame-to-frame; an in-diagonal center still associates. This is
    /// the *base* multiplier; the live gate widens with the filter's position
    /// uncertainty (see `max_gate_dist`).
    pub max_center_dist_diag: f32,
    /// How strongly the filter's own position uncertainty widens the gate.
    /// During a coast the position covariance grows, so the gate should grow
    /// with it (a missed object can be further from the last prediction). In
    /// units of standard deviations of the position estimate added to the base
    /// gate. Zero reproduces a fixed gate.
    pub gate_sigma: f32,
    /// Detections below this confidence are ignored entirely.
    pub min_confidence: f32,
    /// Consecutive associated frames before a tentative track is confirmed.
    pub confirm_hits: u32,
    /// Consecutive missed frames a confirmed track coasts (predict-only) before
    /// it is declared lost. Brief occlusion stays inside this budget and keeps
    /// the id; exceeding it drops the lock.
    pub max_coast_frames: u32,
    /// Measurement noise: how much a single detection box is trusted versus the
    /// prediction. Higher ⇒ smoother (trust the model more), lower ⇒ snappier
    /// (trust the detection more). In the same units as the bbox (pixels).
    pub measurement_noise: f32,
    /// Process noise: how much the motion model is allowed to drift per frame.
    /// Higher ⇒ the filter adapts faster to acceleration / turns.
    pub process_noise: f32,
    /// Largest velocity (pixels/frame, per box component) a single Kalman update
    /// may inject into the estimate. One jittered detection box must not be able
    /// to slam a huge velocity into the filter (which would then overshoot the
    /// next prediction and shake the lock loose). Clamping the per-update
    /// velocity change keeps a half-box-width jitter from poisoning the model.
    pub max_velocity: f32,
    /// Minimum appearance similarity for a candidate to be eligible as the
    /// locked object. A candidate that looks too unlike the template is rejected
    /// even if it sits dead-center in the motion gate (the shadowing-distractor
    /// defence). Only applied when a template + the candidate's descriptor are
    /// both present.
    pub min_appearance: f32,
    /// Appearance margin (best similarity minus second-best similarity) below
    /// which two candidates are treated as appearance-AMBIGUOUS: the lock will
    /// not silently pick one, it goes Uncertain and requests re-designation.
    /// This is the never-silent guard for two identical-looking objects.
    pub ambiguous_margin: f32,
    /// Template update rate (EMA factor in `0..=1`) applied to the locked
    /// object's appearance on a confident measured association. Small ⇒ the
    /// template adapts slowly to lighting / pose, so a one-frame wrong match
    /// cannot snap the template onto a distractor.
    pub template_lr: f32,
    /// Weights for the combined association score: (appearance, motion, detector
    /// confidence). Used only as the TIE-BREAK among candidates whose appearance
    /// similarity is within [`Self::ambiguous_margin`] of the best, appearance
    /// alone owns the primary ranking (see `SingleObjectTracker::decide`), so a
    /// louder/closer distractor cannot out-rank a strictly-more-similar candidate
    /// through this score. Normalized internally, so only the ratio matters.
    pub score_weights: ScoreWeights,
}

/// Relative weights of the three association cues, applied only as the tie-break
/// among appearance-indistinguishable candidates. Appearance owns the primary
/// ranking outright, so these never let a geometric or confidence coincidence
/// outvote a clear appearance preference.
#[derive(Debug, Clone, Copy)]
pub struct ScoreWeights {
    pub appearance: f32,
    pub motion: f32,
    pub confidence: f32,
}

impl Default for ScoreWeights {
    fn default() -> Self {
        Self {
            appearance: 0.6,
            motion: 0.3,
            confidence: 0.1,
        }
    }
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            min_iou: 0.1,
            max_center_dist_diag: 1.5,
            gate_sigma: 3.0,
            min_confidence: 0.0,
            confirm_hits: 2,
            max_coast_frames: 8,
            measurement_noise: 4.0,
            process_noise: 1.0,
            max_velocity: 40.0,
            min_appearance: 0.5,
            ambiguous_margin: 0.08,
            template_lr: 0.1,
            score_weights: ScoreWeights::default(),
        }
    }
}

/// The observable lifecycle of the single locked track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackState {
    /// No track. The next confident detection seeds a tentative one.
    Idle,
    /// A track exists but has not yet been confirmed (too few hits). It is not
    /// reported on the wire so a spurious one-frame blob never claims an id.
    Tentative,
    /// Confirmed and reported. The id is live and the identity is trusted.
    Confirmed,
    /// Confirmed but currently coasting on prediction through a miss, OR held on
    /// the prediction because the frame was appearance-ambiguous, OR re-confirming
    /// after a coast (a re-association coming out of a coast spends at least one
    /// frame here, reported Uncertain, before it is trusted as Confirmed again, so
    /// a handover laundered during a coast surfaces doubt for a frame). The id is
    /// held; downstream sees the predicted/measured box flagged Uncertain.
    Coasting,
}

/// What a single advance produced.
#[derive(Debug, Clone)]
pub struct TrackUpdate {
    /// The lifecycle state after this frame.
    pub state: TrackState,
    /// The reported detection for the locked object this frame, with its
    /// `track_id`, `lock_state`, and `assoc_confidence` filled, or `None` when
    /// there is nothing live to report (idle, or tentative-not-yet-confirmed).
    /// For a coasting / ambiguous frame this is the predicted box.
    pub detection: Option<Detection>,
    /// The track id the lock currently holds, if any.
    pub track_id: Option<u64>,
    /// True when this update came from a measured (and accepted) detection, not
    /// a coast / ambiguous hold.
    pub measured: bool,
    /// True when the tracker could not safely resolve the identity this frame
    /// (two appearance-ambiguous candidates) and is asking the operator layer to
    /// re-designate the target. A silent capture never happens; this flag is how
    /// the ambiguity surfaces.
    pub needs_redesignation: bool,
    /// True only when the reported track was seeded by an explicit operator
    /// [`SingleObjectTracker::designate`](super::SingleObjectTracker::designate) call. A consumer publish path gates the
    /// wire `lock_state` on this: an auto-seeded track (`false`) is tracked and
    /// carries an id, but is never presented as locked, mirroring the offload
    /// publish path. `false` on a frame with nothing to report.
    pub operator_designated: bool,
}
