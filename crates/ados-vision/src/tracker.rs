//! A single-object visual tracker with an appearance model.
//!
//! Detectors are noisy: they jitter a box around an object, occasionally miss a
//! frame, and report several objects when only one is the one a consumer locked
//! onto (follow-me, cinematography framing, an inspection-target lock). This
//! module turns that noisy per-frame stream into a single stable identity.
//!
//! It takes the per-frame detections (the canonical
//! [`ados_protocol::framebus::Detection`] — never a parallel type), predicts
//! where the locked object should be next with a constant-velocity motion
//! model, associates the single best detection by a COMBINED score (appearance
//! similarity first, then motion-gate fit, then detector confidence), and
//! stamps a persistent [`ados_protocol::framebus::Detection::track_id`] on the
//! survivor. A brief miss (a few frames of occlusion or a dropped detection) is
//! coasted on the prediction and keeps the id; only a real loss drops the lock
//! and lets a re-acquire mint a fresh id.
//!
//! ## Why appearance, and where it comes from
//!
//! A gate that scores purely on geometry cannot tell two boxes apart when they
//! overlap: a distractor that rides the prediction matches the gate as well as
//! (or better than) the true target that maneuvered off it, so a best-IoU pick
//! silently captures the wrong object. The fix is to also compare what the box
//! *looks like*. This module captures the locked object's appearance template at
//! confirm and matches candidates by appearance similarity, updating the
//! template slowly so it cannot drift onto a distractor.
//!
//! The appearance descriptor is intentionally NOT on the wire [`Detection`]
//! (raw embeddings would bloat every detection message). It travels through the
//! tracker's own [`Candidate`] type as a side input. A real deployment extracts
//! the descriptor from the candidate bbox's pixels via an [`AppearanceModel`]
//! (a colour/gradient histogram now, a learned re-id / siamese embedding at
//! Gate-B — the trait makes that swap source-compatible). The synthetic test
//! harness supplies a descriptor per detection directly, so association can be
//! exercised without pixels: a target and a distractor get *different*
//! descriptors, exactly as two real objects would.
//!
//! ## The never-silent doctrine
//!
//! Association is appearance-first: among the candidates that clear the absolute
//! appearance gate, the one that looks MOST like the template wins, and motion /
//! detector-confidence only break a tie among candidates whose appearance is
//! within an ambiguity margin of the best. A louder or better-positioned
//! distractor can therefore never demote a candidate that is strictly more
//! appearance-similar than it — the cue that owns identity (appearance) is not
//! out-voted by the cues that only describe where a box sits and how confident
//! the detector was about its class.
//!
//! When two admissible candidates are within that appearance margin of each
//! other they are indistinguishable by appearance, so the tracker does NOT
//! silently pick one: the lock goes [`LockState::Uncertain`], holds/coasts on the
//! prediction, lowers its confidence, and raises a re-designation request
//! ([`SingleObjectTracker::needs_redesignation`]) so the operator layer can
//! require an explicit re-confirm before the lock is trusted again.
//!
//! Silent capture is PREVENTED for the cases vision can actually adjudicate: a
//! distinguishable distractor (it loses the appearance race) and two co-visible
//! appearance-ambiguous candidates (the never-silent hold fires). The one case
//! that remains is a lone impostor that is appearance-identical to the target
//! while the true target is absent from frame: that is information-theoretically
//! unsolvable from vision alone (there is no signal to separate them), and it is
//! handled out of band by the operator-re-designation backstop, not by a guess.
//!
//! The core is pure and deterministic — it owns no camera, socket, or clock. It
//! advances on whatever per-frame candidate list it is handed, so the same input
//! always yields the same id assignment.

use ados_protocol::framebus::{BoundingBox, Detection, LockState};

/// Length of the model-free appearance descriptor the interim
/// [`HistogramAppearance`] produces. Small and fixed so it is cheap to compare
/// and carry; a learned embedding would pick its own length behind the trait.
pub const APPEARANCE_DIM: usize = 16;

/// An appearance descriptor for one candidate box.
///
/// This is a normalized feature vector: the model-free interim builds it from
/// coarse colour/gradient statistics of the box pixels; a learned re-id model
/// would emit an L2-normalized embedding of the same shape. It is compared with
/// [`AppearanceModel::similarity`], not by raw equality, so two descriptors of
/// the *same* object (under jitter / lighting change) score high and two
/// *different* objects score low.
#[derive(Debug, Clone, PartialEq)]
pub struct Appearance {
    /// The feature vector. Length is model-defined; the interim uses
    /// [`APPEARANCE_DIM`].
    pub features: Vec<f32>,
}

impl Appearance {
    /// Build from a raw feature slice (the harness and a real extractor both use
    /// this). The vector is stored as-is; [`AppearanceModel::similarity`] owns
    /// any normalization.
    pub fn from_features(features: Vec<f32>) -> Self {
        Self { features }
    }
}

/// Pluggable appearance backend.
///
/// `extract` turns a candidate box (its pixels, in a real impl) into an
/// [`Appearance`]; `similarity` scores two descriptors in `0..=1` (1 = identical
/// appearance, 0 = unrelated). The tracker depends only on this trait, so the
/// model-free [`HistogramAppearance`] shipped here can be swapped for a learned
/// re-id / siamese embedding at Gate-B with no tracker change: the new model
/// just produces longer descriptors and a learned similarity.
///
/// The default `extract` returns `None` (no pixels available — the pure /
/// synthetic path), so a tracker can run appearance-aware on descriptors handed
/// to it directly (the harness) while a deployment plugs in a real extractor.
pub trait AppearanceModel: Send + Sync {
    /// Extract a descriptor for the box `bbox` from the frame `pixels`. Returns
    /// `None` when no frame is available (the model-free / synthetic path), in
    /// which case the caller must supply the descriptor on the [`Candidate`].
    fn extract(&self, _pixels: Option<&[u8]>, _bbox: &BoundingBox) -> Option<Appearance> {
        None
    }

    /// Similarity of two descriptors in `0..=1`. Higher = more alike.
    fn similarity(&self, a: &Appearance, b: &Appearance) -> f32;
}

/// The model-free interim appearance backend: cosine similarity over the
/// descriptor vectors.
///
/// It does NOT extract from pixels (a real deployment swaps in a re-id model for
/// that at Gate-B); it scores descriptors the caller already holds. Cosine
/// similarity is scale-invariant and maps cleanly onto `0..=1` after clamping,
/// so a colour/gradient histogram now and a learned embedding later both work
/// through the same `similarity`. Two equal vectors score 1.0; orthogonal ones
/// score 0.0.
#[derive(Debug, Clone, Copy, Default)]
pub struct HistogramAppearance;

impl AppearanceModel for HistogramAppearance {
    fn similarity(&self, a: &Appearance, b: &Appearance) -> f32 {
        cosine_similarity(&a.features, &b.features)
    }
}

/// Cosine similarity clamped to `0..=1` (negative correlation reads as "no
/// match", not as anti-match). A zero-magnitude vector yields 0.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= 0.0 || nb <= 0.0 {
        return 0.0;
    }
    (dot / (na.sqrt() * nb.sqrt())).clamp(0.0, 1.0)
}

/// One candidate detection for a frame, optionally carrying its appearance
/// descriptor.
///
/// The motion-only [`SingleObjectTracker::update`] path wraps each wire
/// [`Detection`] with `appearance: None`; the appearance-aware
/// [`SingleObjectTracker::update_with_appearance`] path carries a descriptor per
/// candidate (extracted from pixels in a real deployment, supplied directly by
/// the synthetic harness). The descriptor never reaches the wire — only the
/// resulting lock-state / association confidence does.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// The detection as it would appear on the wire (no appearance field).
    pub detection: Detection,
    /// Appearance descriptor for this box, when available.
    pub appearance: Option<Appearance>,
}

impl Candidate {
    /// A candidate with no appearance descriptor (motion-only path).
    pub fn motion_only(detection: Detection) -> Self {
        Self {
            detection,
            appearance: None,
        }
    }

    /// A candidate carrying its appearance descriptor.
    pub fn with_appearance(detection: Detection, appearance: Appearance) -> Self {
        Self {
            detection,
            appearance: Some(appearance),
        }
    }
}

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
    /// uncertainty (see [`max_gate_dist`]).
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
    /// similarity is within [`Self::ambiguous_margin`] of the best — appearance
    /// alone owns the primary ranking (see [`SingleObjectTracker::decide`]), so a
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
    /// [`SingleObjectTracker::designate`] call. A consumer publish path gates the
    /// wire `lock_state` on this: an auto-seeded track (`false`) is tracked and
    /// carries an id, but is never presented as locked, mirroring the offload
    /// publish path. `false` on a frame with nothing to report.
    pub operator_designated: bool,
}

/// A constant-velocity Kalman filter over a bounding box.
///
/// State is `[cx, cy, w, h, vx, vy, vw, vh]` — the box center, size, and their
/// per-frame velocities. Each of the four box components is an independent
/// 1-D constant-velocity sub-filter (position + velocity), which keeps the math
/// small, allocation-free, and exact without a matrix library while giving the
/// same predict/update behaviour as the full 8-state form for this layout.
#[derive(Debug, Clone)]
struct BoxKalman {
    /// Position estimate per component: cx, cy, w, h.
    pos: [f32; 4],
    /// Velocity estimate per component.
    vel: [f32; 4],
    /// Estimate covariance per component: [[p_pos, p_pv], [p_pv, p_vel]].
    p_pos: [f32; 4],
    p_vel: [f32; 4],
    p_pv: [f32; 4],
    measurement_noise: f32,
    process_noise: f32,
    max_velocity: f32,
}

impl BoxKalman {
    fn new(b: &BoundingBox, cfg: &TrackerConfig) -> Self {
        let (cx, cy) = center(b);
        Self {
            pos: [cx, cy, b.width, b.height],
            vel: [0.0; 4],
            // Start moderately uncertain in position, very uncertain in velocity
            // (we have not observed motion yet).
            p_pos: [cfg.measurement_noise; 4],
            p_vel: [1_000.0; 4],
            p_pv: [0.0; 4],
            measurement_noise: cfg.measurement_noise,
            process_noise: cfg.process_noise,
            max_velocity: cfg.max_velocity,
        }
    }

    /// Advance the state by one frame (dt is folded into the tuned process noise
    /// for a fixed-rate stream; we treat one advance as one unit step).
    fn predict(&mut self) {
        let q = self.process_noise;
        for i in 0..4 {
            // x = F x : pos += vel, vel unchanged.
            self.pos[i] += self.vel[i];
            // P = F P F^T + Q for the 2x2 constant-velocity block.
            let pp = self.p_pos[i];
            let pv = self.p_vel[i];
            let ppv = self.p_pv[i];
            self.p_pos[i] = pp + 2.0 * ppv + pv + q;
            self.p_pv[i] = ppv + pv;
            self.p_vel[i] = pv + q;
        }
    }

    /// Fuse a measured box into the state (the standard scalar Kalman update on
    /// each component's position observation).
    ///
    /// The velocity correction from a single update is clamped to
    /// `max_velocity`: a badly jittered box can otherwise inject an enormous
    /// velocity (the initial velocity covariance is large, so the gain on
    /// velocity is near 1), and that poisoned velocity overshoots the next
    /// prediction and shakes the lock loose. Bounding the learned velocity per
    /// step is what lets the filter hold one id through half-box-width jitter.
    fn update(&mut self, b: &BoundingBox) {
        let (cx, cy) = center(b);
        let meas = [cx, cy, b.width, b.height];
        let r = self.measurement_noise;
        let vmax = self.max_velocity;
        for (i, &m) in meas.iter().enumerate() {
            let s = self.p_pos[i] + r; // innovation covariance
            let k_pos = self.p_pos[i] / s; // Kalman gain, position
            let k_vel = self.p_pv[i] / s; // Kalman gain, velocity
            let innov = m - self.pos[i];
            self.pos[i] += k_pos * innov;
            // Clamp the velocity the filter is allowed to learn from one update.
            let new_vel = (self.vel[i] + k_vel * innov).clamp(-vmax, vmax);
            self.vel[i] = new_vel;
            // P = (I - K H) P
            let pp = self.p_pos[i];
            let ppv = self.p_pv[i];
            self.p_pos[i] = pp - k_pos * pp;
            self.p_pv[i] = ppv - k_pos * ppv;
            self.p_vel[i] -= k_vel * ppv;
        }
    }

    /// Position standard deviation of the center (mean of cx, cy components).
    /// Used to widen the association gate as the estimate gets less certain
    /// (during a coast the covariance grows, so the gate should too).
    fn center_pos_std(&self) -> f32 {
        let var = (self.p_pos[0] + self.p_pos[1]) * 0.5;
        var.max(0.0).sqrt()
    }

    /// The current box estimate (center+size back to top-left x/y).
    fn bbox(&self) -> BoundingBox {
        let w = self.pos[2].max(1.0);
        let h = self.pos[3].max(1.0);
        BoundingBox {
            x: self.pos[0] - w / 2.0,
            y: self.pos[1] - h / 2.0,
            width: w,
            height: h,
        }
    }
}

/// The single-object tracker. Holds at most one track at a time.
pub struct SingleObjectTracker {
    cfg: TrackerConfig,
    track: Option<Track>,
    next_id: u64,
    appearance: Box<dyn AppearanceModel>,
    /// Latched while the current lock is asking for operator re-designation
    /// (cleared on a fresh seed or an explicit [`Self::redesignate`]).
    needs_redesignation: bool,
}

impl std::fmt::Debug for SingleObjectTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SingleObjectTracker")
            .field("cfg", &self.cfg)
            .field("track", &self.track)
            .field("next_id", &self.next_id)
            .field("needs_redesignation", &self.needs_redesignation)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct Track {
    id: u64,
    kf: BoxKalman,
    state: TrackState,
    /// Consecutive associated frames (for tentative→confirmed).
    hits: u32,
    /// Consecutive missed frames (for coast→lost).
    misses: u32,
    /// Class label of the locked object, carried through coasts.
    class_label: String,
    /// Last measured confidence, carried through coasts.
    confidence: f32,
    /// Last association confidence stamped on the wire (carried into a coast).
    assoc_confidence: f32,
    /// The locked object's appearance template, updated slowly on confident
    /// associations. `None` until the first descriptor is seen.
    template: Option<Appearance>,
    /// Set when a re-association just brought the track out of a MISS-induced
    /// coast. The first such re-association stays Coasting (reported Uncertain)
    /// for one transitional re-confirm frame instead of snapping to Confirmed;
    /// the next clean association clears this and confirms. This makes a handover
    /// laundered during an occlusion surface doubt for at least one frame.
    reconfirming: bool,
    /// True while the current coast is an APPEARANCE-AMBIGUITY hold (two
    /// indistinguishable candidates) rather than a genuine miss. An ambiguity
    /// hold that is then resolved by a single lone admissible candidate (the
    /// competitor left frame) re-locks immediately — the ambiguity was resolved
    /// by the competitor's disappearance, so no extra re-confirm frame is owed.
    /// Only a miss-coast (the laundering threat) takes the re-confirm window.
    coast_ambiguous: bool,
    /// True only when this track originates from an explicit operator
    /// [`SingleObjectTracker::designate`] call. An automatically seeded track
    /// (the most-confident auto-pick) is `false`: it is still tracked for
    /// continuity — it keeps its id and predicted box — but a consumer publish
    /// path must NOT present it as locked. Only a target the operator actually
    /// designated carries a lock state to consumers, so a follow behavior can
    /// never engage a subject nobody chose. This mirrors the offload publish
    /// path, which stamps a lock state only on the designated track.
    operator_designated: bool,
}

impl SingleObjectTracker {
    /// A tracker with the model-free interim appearance backend
    /// ([`HistogramAppearance`]).
    pub fn new(cfg: TrackerConfig) -> Self {
        Self::with_appearance_model(cfg, Box::new(HistogramAppearance))
    }

    /// A tracker with a chosen appearance backend. At Gate-B a learned re-id /
    /// siamese embedding implementing [`AppearanceModel`] plugs in here with no
    /// other change.
    pub fn with_appearance_model(cfg: TrackerConfig, appearance: Box<dyn AppearanceModel>) -> Self {
        Self {
            cfg,
            track: None,
            next_id: 1,
            appearance,
            needs_redesignation: false,
        }
    }

    /// The id the lock currently holds, if confirmed or coasting.
    pub fn current_id(&self) -> Option<u64> {
        self.track.as_ref().and_then(|t| match t.state {
            TrackState::Confirmed | TrackState::Coasting => Some(t.id),
            _ => None,
        })
    }

    /// The lifecycle state.
    pub fn state(&self) -> TrackState {
        self.track.as_ref().map_or(TrackState::Idle, |t| t.state)
    }

    /// True while the tracker is asking the operator to re-designate the target
    /// because the identity could not be resolved safely (two appearance-
    /// ambiguous candidates). Composes with an operator re-acknowledge step:
    /// the operator layer should require an explicit re-confirm before trusting
    /// the lock again. The flag clears on a clean re-association or an explicit
    /// [`Self::redesignate`].
    pub fn needs_redesignation(&self) -> bool {
        self.needs_redesignation
    }

    /// Operator re-designation hook: clear the ambiguity latch (the operator has
    /// re-confirmed the target out of band). Does not move the lock; the next
    /// frame re-associates from the held state.
    pub fn redesignate(&mut self) {
        self.needs_redesignation = false;
    }

    /// Operator designation: drop any current lock and seed a fresh track onto a
    /// specific detection (the box the operator clicked), instead of the
    /// most-confident one — the operator's pick overrides the confidence floor.
    /// Returns the new track id. The next [`update`] associates from this seed.
    /// Used by the click-to-follow path so the operator picks the target rather
    /// than the tracker's auto-lock.
    ///
    /// [`update`]: Self::update
    pub fn designate(&mut self, detection: &Detection) -> Option<u64> {
        self.track = None;
        let cand = Candidate::motion_only(detection.clone());
        let _ = self.seed(&[&cand], true);
        self.track.as_ref().map(|t| t.id)
    }

    /// Advance one frame on a list of wire detections, motion-only (no
    /// appearance descriptors). Each detection is wrapped as a [`Candidate`]
    /// with no appearance, so association falls back to the motion gate plus the
    /// class-continuity guard. Kept for callers / tests that have no descriptor
    /// source; the appearance defence is inactive on this path.
    pub fn update(&mut self, detections: &[Detection]) -> TrackUpdate {
        let candidates: Vec<Candidate> = detections
            .iter()
            .cloned()
            .map(Candidate::motion_only)
            .collect();
        self.update_with_appearance(&candidates)
    }

    /// Advance one frame on a list of [`Candidate`]s carrying appearance
    /// descriptors. This is the real path: the engine extracts a descriptor per
    /// candidate from the frame pixels (Gate-B) or the synthetic harness
    /// supplies one. Association is appearance-first (the most appearance-similar
    /// admissible candidate wins; motion + confidence only break an
    /// appearance tie), the appearance gate rejects shadowing distractors, and
    /// the ambiguity guard refuses to silently pick between two
    /// appearance-indistinguishable objects.
    pub fn update_with_appearance(&mut self, candidates: &[Candidate]) -> TrackUpdate {
        // Filter out detections below the confidence floor up front.
        let eligible: Vec<&Candidate> = candidates
            .iter()
            .filter(|c| c.detection.confidence >= self.cfg.min_confidence)
            .collect();

        match self.track.take() {
            None => self.seed(&eligible, false),
            Some(track) => self.advance(track, &eligible),
        }
    }

    /// No track yet: lock onto the most confident eligible detection (if any),
    /// capturing its appearance template. `operator_designated` is `true` only
    /// when the seed comes from an explicit [`Self::designate`] call (the
    /// operator's pick); the automatic most-confident seed passes `false`, and
    /// that flag rides the track so a consumer publish path can withhold the
    /// lock state from an auto-seeded target.
    fn seed(&mut self, candidates: &[&Candidate], operator_designated: bool) -> TrackUpdate {
        // A fresh seed clears any pending re-designation: we are starting over.
        self.needs_redesignation = false;
        let best = candidates
            .iter()
            .copied()
            .max_by(|a, b| total_cmp(a.detection.confidence, b.detection.confidence));
        match best {
            None => TrackUpdate {
                state: TrackState::Idle,
                detection: None,
                track_id: None,
                measured: false,
                needs_redesignation: false,
                operator_designated: false,
            },
            Some(cand) => {
                let det = &cand.detection;
                let id = self.next_id;
                self.next_id += 1;
                let template = self.template_for(cand);
                let track = Track {
                    id,
                    kf: BoxKalman::new(&det.bbox, &self.cfg),
                    // One hit is not yet enough to confirm (default confirm_hits
                    // is 2), so a single spurious blob never claims an id.
                    state: if self.cfg.confirm_hits <= 1 {
                        TrackState::Confirmed
                    } else {
                        TrackState::Tentative
                    },
                    hits: 1,
                    misses: 0,
                    class_label: det.class_label.clone(),
                    confidence: det.confidence,
                    assoc_confidence: det.confidence,
                    template,
                    reconfirming: false,
                    coast_ambiguous: false,
                    operator_designated,
                };
                let state = track.state;
                let report = if state == TrackState::Confirmed {
                    Some(reported(
                        &track,
                        track.kf.bbox(),
                        LockState::Locked,
                        det.confidence,
                    ))
                } else {
                    None
                };
                let id_out = self.current_id_for(&track);
                self.track = Some(track);
                TrackUpdate {
                    state,
                    detection: report,
                    track_id: id_out,
                    measured: true,
                    needs_redesignation: false,
                    operator_designated,
                }
            }
        }
    }

    /// A track exists: predict, score in-gate candidates by the combined cue,
    /// associate / coast / hold-on-ambiguity, and run the lifecycle.
    fn advance(&mut self, mut track: Track, candidates: &[&Candidate]) -> TrackUpdate {
        track.kf.predict();
        let predicted = track.kf.bbox();
        let gate = self.max_gate_dist(&track, &predicted);

        // Score every in-gate candidate. The decision uses the best score, the
        // appearance gate, and the appearance margin to the runner-up.
        let scored = self.score_in_gate(&track, &predicted, gate, candidates);

        match self.decide(&track, &scored) {
            Decision::Associate { idx, blend } => {
                let cand = scored[idx].cand;
                let det = &cand.detection;
                let assoc = scored[idx].assoc_confidence;
                // A re-association out of a genuine MISS-induced coast owes a
                // transitional re-confirm frame; one out of an appearance-
                // ambiguity hold that is now resolved by a lone candidate does
                // not (the ambiguity was resolved by the competitor leaving).
                let was_miss_coast = track.state == TrackState::Coasting && !track.coast_ambiguous;
                track.kf.update(&det.bbox);
                track.hits += 1;
                track.misses = 0;
                // Class-continuity guard: do not silently adopt a different
                // class label. Keep the original on a class change (the
                // ambiguity / appearance gate already vetted the box).
                if det.class_label == track.class_label {
                    track.confidence = det.confidence;
                } else {
                    // Treat a class change as a weak association: keep the id but
                    // lower the reported confidence and do not overwrite the
                    // class. A persistent change would have to keep re-passing
                    // the appearance gate to stay locked.
                    track.confidence = det.confidence.min(track.confidence);
                }
                track.assoc_confidence = assoc;
                // The ambiguity (if any) is being resolved by this association;
                // clear the ambiguity-coast marker.
                track.coast_ambiguous = false;
                // Re-association coming OUT of a miss-induced coast must pass
                // through a transitional re-confirm frame (reported Uncertain)
                // before it is trusted as Confirmed again — a handover laundered
                // during an occlusion surfaces doubt for at least one frame
                // instead of snapping straight Coasting→Confirmed.
                let entering_reconfirm = was_miss_coast && !track.reconfirming;
                // Suppress template poisoning: only walk the EMA on a clean,
                // unambiguous association whose chosen candidate is the clear
                // appearance winner (no near-competitor) AND that is not the
                // first, still-doubted re-confirm frame out of a coast. A frame
                // under any contention or doubt leaves the template untouched, so
                // the EMA can never be dragged onto a prediction-riding
                // distractor and self-reinforce.
                if blend && !entering_reconfirm {
                    self.blend_template(&mut track, cand);
                }
                track.state = match track.state {
                    TrackState::Tentative if track.hits >= self.cfg.confirm_hits => {
                        TrackState::Confirmed
                    }
                    // First re-association out of a coast: stay Coasting for one
                    // transitional re-confirm frame.
                    TrackState::Coasting if entering_reconfirm => {
                        track.reconfirming = true;
                        TrackState::Coasting
                    }
                    // The re-confirm window is satisfied by this clean
                    // association: confirm.
                    TrackState::Coasting => TrackState::Confirmed,
                    s => s,
                };
                let state = track.state;
                let measured;
                let report = if matches!(state, TrackState::Confirmed) {
                    // A clean, trusted association clears the ambiguity latch and
                    // the re-confirm flag.
                    self.needs_redesignation = false;
                    track.reconfirming = false;
                    measured = true;
                    Some(reported(&track, track.kf.bbox(), LockState::Locked, assoc))
                } else if matches!(state, TrackState::Coasting) {
                    // The transitional re-confirm frame: a measurement WAS taken
                    // (the box tracks it) but the identity is not yet re-trusted,
                    // so it reports Uncertain and does not count as a resolved
                    // measurement. Keep any pending re-designation pending.
                    measured = false;
                    Some(reported(
                        &track,
                        track.kf.bbox(),
                        LockState::Uncertain,
                        (assoc * 0.75).clamp(0.0, 1.0),
                    ))
                } else {
                    // Still Tentative: not yet reported.
                    self.needs_redesignation = false;
                    measured = true;
                    None
                };
                let id_out = self.current_id_for(&track);
                let needs = self.needs_redesignation;
                let operator_designated = track.operator_designated;
                self.track = Some(track);
                TrackUpdate {
                    state,
                    detection: report,
                    track_id: id_out,
                    measured,
                    needs_redesignation: needs,
                    operator_designated,
                }
            }
            Decision::Ambiguous(assoc) => {
                // Two appearance-ambiguous candidates: NEVER silently pick one.
                // Hold on the prediction, flag re-designation, keep the id while
                // within the coast budget. This is the never-silent backstop.
                track.misses += 1;
                track.hits = 0;
                self.hold_or_drop(track, predicted, assoc, true)
            }
            Decision::None => {
                // No association this frame.
                track.misses += 1;
                track.hits = 0;
                match track.state {
                    // A tentative track that misses before confirming is dropped
                    // immediately — it never earned an id, so nothing to hold.
                    TrackState::Tentative => {
                        self.needs_redesignation = false;
                        TrackUpdate {
                            state: TrackState::Idle,
                            detection: None,
                            track_id: None,
                            measured: false,
                            needs_redesignation: false,
                            operator_designated: track.operator_designated,
                        }
                    }
                    TrackState::Confirmed | TrackState::Coasting => {
                        let assoc = track.assoc_confidence;
                        self.hold_or_drop(track, predicted, assoc, false)
                    }
                    TrackState::Idle => unreachable!("advancing a track in Idle"),
                }
            }
        }
    }

    /// Coast (or, on the budget edge, drop) a confirmed/coasting track that did
    /// not get a clean association this frame. `ambiguous` distinguishes a
    /// genuine miss (occlusion) from an appearance-ambiguity hold (two
    /// identical-looking candidates) — the latter raises re-designation.
    fn hold_or_drop(
        &mut self,
        mut track: Track,
        predicted: BoundingBox,
        prev_assoc: f32,
        ambiguous: bool,
    ) -> TrackUpdate {
        if track.misses > self.cfg.max_coast_frames {
            // Real loss: drop the lock. A future detection re-acquires under a
            // fresh id. Clear the ambiguity latch — there is no lock to confirm.
            self.needs_redesignation = false;
            // Emit one final Lost event on the wire for the track that just
            // dropped: the predicted box carries the OLD id flagged
            // `LockState::Lost` so a downstream consumer sees the loss explicitly
            // (this is where the wire enum's `Lost` is produced). The live held id
            // (`TrackUpdate::track_id`) is cleared — the lock is gone — so a
            // drop+re-acquire still travels through a None-id gap and never reads
            // as an in-lock swap.
            let lost = reported(&track, predicted, LockState::Lost, 0.0);
            return TrackUpdate {
                state: TrackState::Idle,
                detection: Some(lost),
                track_id: None,
                measured: false,
                needs_redesignation: false,
                operator_designated: track.operator_designated,
            };
        }
        track.state = TrackState::Coasting;
        // Remember whether this coast is an ambiguity hold or a genuine miss, so
        // the re-association out of it knows whether it owes a re-confirm frame.
        track.coast_ambiguous = ambiguous;
        // Association confidence decays with the coast length so a long hold
        // reads as a weak identity claim on the wire.
        let assoc = (prev_assoc / (1.0 + track.misses as f32)).clamp(0.0, 1.0);
        track.assoc_confidence = assoc;
        if ambiguous {
            self.needs_redesignation = true;
        }
        let report = reported(&track, predicted, LockState::Uncertain, assoc);
        let id_out = Some(track.id);
        let needs = self.needs_redesignation;
        let operator_designated = track.operator_designated;
        self.track = Some(track);
        TrackUpdate {
            state: TrackState::Coasting,
            detection: Some(report),
            track_id: id_out,
            measured: false,
            needs_redesignation: needs,
            operator_designated,
        }
    }

    /// The live association gate distance: the base center-distance gate widened
    /// by the filter's position uncertainty. During a coast the covariance
    /// grows, so the gate grows with it; a confidently-tracked object has a
    /// tight gate.
    fn max_gate_dist(&self, track: &Track, predicted: &BoundingBox) -> f32 {
        let diag = (predicted.width * predicted.width + predicted.height * predicted.height).sqrt();
        let base = diag * self.cfg.max_center_dist_diag;
        let sigma = self.cfg.gate_sigma * track.kf.center_pos_std();
        base + sigma
    }

    /// Score every in-gate candidate against the track. Eligibility is the
    /// geometric gate (IoU ≥ min_iou OR center within the widened distance); each
    /// eligible candidate carries its appearance similarity, a combined
    /// appearance + motion + confidence score (the motion-only ranking key), and
    /// the association confidence to stamp if chosen. The appearance-first
    /// ranking and the admissibility / ambiguity decisions happen in
    /// [`Self::decide`].
    fn score_in_gate<'a>(
        &self,
        track: &Track,
        predicted: &BoundingBox,
        gate: f32,
        candidates: &[&'a Candidate],
    ) -> Vec<Scored<'a>> {
        let (pcx, pcy) = center(predicted);
        let mut out: Vec<Scored<'a>> = Vec::new();
        for cand in candidates {
            let det = &cand.detection;
            let geo_iou = iou(predicted, &det.bbox);
            let (dcx, dcy) = center(&det.bbox);
            let dist = ((dcx - pcx).powi(2) + (dcy - pcy).powi(2)).sqrt();
            let in_gate = geo_iou >= self.cfg.min_iou || dist <= gate;
            if !in_gate {
                continue;
            }
            // Motion fit: 1.0 dead-on the prediction, decaying with distance
            // across the gate. IoU sharpens it when the boxes overlap.
            let motion = motion_fit(geo_iou, dist, gate);
            // Appearance similarity to the template (1.0 when we have no
            // template or no descriptor — appearance is simply not a cue then,
            // it neither helps nor vetoes).
            let (appearance, has_app) = match (&track.template, &cand.appearance) {
                (Some(t), Some(a)) => (self.appearance.similarity(t, a), true),
                _ => (1.0, false),
            };
            let w = self.cfg.score_weights;
            let wsum = (w.appearance + w.motion + w.confidence).max(f32::EPSILON);
            let score =
                (w.appearance * appearance + w.motion * motion + w.confidence * det.confidence)
                    / wsum;
            // Association confidence: when appearance is a live cue, anchor it on
            // the appearance similarity tempered by motion; otherwise fall back
            // to the detector confidence (motion-only path).
            let assoc_confidence = if has_app {
                (appearance * 0.5 + motion * 0.3 + det.confidence * 0.2).clamp(0.0, 1.0)
            } else {
                det.confidence
            };
            out.push(Scored {
                cand,
                score,
                appearance,
                has_app,
                assoc_confidence,
            });
        }
        // Order by combined score for determinism / debuggability. The decision
        // is appearance-first (see `decide`), so this ordering is NOT the
        // association key on the appearance path — it only ranks the motion-only
        // fallback, where the combined score IS the decision.
        out.sort_by(|a, b| total_cmp(b.score, a.score));
        out
    }

    /// Decide what to do with the scored, in-gate candidates.
    ///
    /// Association is APPEARANCE-FIRST. When appearance is a live cue (a template
    /// exists and the candidate carries a descriptor):
    ///
    /// 1. Only candidates that clear the absolute appearance gate
    ///    ([`TrackerConfig::min_appearance`]) are admissible. A shadowing
    ///    distractor that rides the prediction but looks unlike the template is
    ///    rejected here even though it is dead-center in the motion gate.
    /// 2. Among admissible candidates the one with the highest APPEARANCE
    ///    similarity is the winner. Motion / detector-confidence are used ONLY to
    ///    break a tie among candidates whose appearance is within
    ///    [`TrackerConfig::ambiguous_margin`] of that best appearance. A candidate
    ///    that is strictly more appearance-similar (beyond the margin) can never
    ///    be demoted below a less-similar one by a louder confidence or a better
    ///    motion fit — the cue that owns identity is not out-voted by the cues
    ///    that only say where the box sits.
    /// 3. If a second admissible candidate's appearance is within the margin of
    ///    the best, the two are appearance-indistinguishable — the tracker refuses
    ///    to pick and goes [`Decision::Ambiguous`] (hold + re-designate), exactly
    ///    as the co-visible identical case does.
    ///
    /// When appearance is NOT a live cue (motion-only path, no template / no
    /// descriptor) the combined score ranks the candidates and the best is taken,
    /// reproducing the motion-gate behaviour.
    fn decide(&self, track: &Track, scored: &[Scored<'_>]) -> Decision {
        if scored.is_empty() {
            return Decision::None;
        }

        // Whether appearance is a live cue this frame: a template exists and at
        // least the front-runner carries a descriptor.
        let appearance_live = track.template.is_some() && scored.iter().any(|s| s.has_app);

        if !appearance_live {
            // Motion-only path: combined score already ranked them; take the best.
            // (No appearance template/descriptor means appearance neither helps
            // nor vetoes — the motion gate carries the decision.)
            let best = scored
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| total_cmp(a.score, b.score))
                .map(|(i, _)| i)
                .unwrap_or(0);
            return Decision::Associate {
                idx: best,
                blend: true,
            };
        }

        // Appearance-first path. Admissible = carries a descriptor AND clears the
        // absolute appearance gate.
        let admissible: Vec<usize> = scored
            .iter()
            .enumerate()
            .filter(|(_, s)| s.has_app && s.appearance >= self.cfg.min_appearance)
            .map(|(i, _)| i)
            .collect();

        let Some(&best_idx) = admissible
            .iter()
            .max_by(|&&a, &&b| total_cmp(scored[a].appearance, scored[b].appearance))
        else {
            // Nothing clears the appearance gate → no admissible match (coast).
            return Decision::None;
        };
        let best_app = scored[best_idx].appearance;

        // The appearance-tie group: every admissible candidate whose appearance is
        // within `ambiguous_margin` of the best. Any candidate strictly more than
        // a margin LESS similar is appearance-distinguishable and cannot win.
        let tie_group: Vec<usize> = admissible
            .iter()
            .copied()
            .filter(|&i| (best_app - scored[i].appearance) <= self.cfg.ambiguous_margin)
            .collect();

        // Contention → Uncertain: if two (or more) admissible candidates are in
        // the appearance-tie group, they are indistinguishable by appearance —
        // refuse to silently pick. This fires for the near-target chameleon (its
        // similarity sits within the margin of the target's), not only for
        // cosine-identical pairs.
        if tie_group.len() > 1 {
            // Confidence of the (refused) appearance winner, halved to reflect the
            // unresolved identity, is what the held frame reports.
            return Decision::Ambiguous((scored[best_idx].assoc_confidence * 0.5).clamp(0.0, 1.0));
        }

        // A single, unambiguous appearance winner. (When the tie group has one
        // member there is no appearance tie to break, so motion/confidence play no
        // ranking role — they only ever tie-break WITHIN the group, which here is
        // a single candidate.) Blend is safe: the winner is the clear appearance
        // match with no near-competitor, so the EMA cannot be dragged onto a
        // prediction-riding distractor.
        Decision::Associate {
            idx: best_idx,
            blend: true,
        }
    }

    /// Build the appearance template for a fresh seed from a candidate's
    /// descriptor (when present).
    fn template_for(&self, cand: &Candidate) -> Option<Appearance> {
        cand.appearance.clone()
    }

    /// Blend a confident match's descriptor into the locked object's template
    /// (EMA at `template_lr`), so the template tracks slow lighting / pose change
    /// without ever snapping onto a one-frame wrong match.
    fn blend_template(&self, track: &mut Track, cand: &Candidate) {
        let Some(new_app) = &cand.appearance else {
            return;
        };
        match &mut track.template {
            None => track.template = Some(new_app.clone()),
            Some(t) if t.features.len() == new_app.features.len() => {
                let lr = self.cfg.template_lr.clamp(0.0, 1.0);
                for (acc, &x) in t.features.iter_mut().zip(new_app.features.iter()) {
                    *acc = *acc * (1.0 - lr) + x * lr;
                }
            }
            // Descriptor length changed (a model swap mid-track): adopt the new
            // one wholesale rather than blend mismatched lengths.
            Some(_) => track.template = Some(new_app.clone()),
        }
    }

    fn current_id_for(&self, track: &Track) -> Option<u64> {
        match track.state {
            TrackState::Confirmed | TrackState::Coasting => Some(track.id),
            _ => None,
        }
    }
}

/// A scored, in-gate candidate.
struct Scored<'a> {
    cand: &'a Candidate,
    /// Combined association score (ranking key).
    score: f32,
    /// Appearance similarity to the template (1.0 when appearance is not a cue).
    appearance: f32,
    /// Whether appearance was actually a live cue for this candidate.
    has_app: bool,
    /// Association confidence to stamp on the wire if this one is chosen.
    assoc_confidence: f32,
}

/// The association decision for one frame.
enum Decision {
    /// Associate the candidate at this index in the scored list. `blend` is true
    /// when the association is clean enough to walk the appearance template toward
    /// it (an unambiguous appearance winner with no near-competitor); false
    /// suppresses the EMA so a contended / doubted frame cannot poison the
    /// template.
    Associate { idx: usize, blend: bool },
    /// Two appearance-ambiguous candidates: hold + re-designate. Carries the
    /// association confidence to report on the held frame.
    Ambiguous(f32),
    /// No admissible association: coast.
    None,
}

/// Motion-fit cue in `0..=1`: IoU when the boxes overlap, otherwise a linear
/// falloff of center distance across the gate. A candidate dead-on the
/// prediction scores ~1.0; one at the gate edge scores ~0.
fn motion_fit(geo_iou: f32, dist: f32, gate: f32) -> f32 {
    if geo_iou > 0.0 {
        // Overlapping: IoU is the sharper motion signal.
        geo_iou.clamp(0.0, 1.0)
    } else if gate > 0.0 {
        (1.0 - (dist / gate)).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Build the reported detection for the locked object, with its id, lock state,
/// and association confidence stamped on the canonical wire type.
///
/// `lock` carries the identity certainty onto the wire: a measured, resolved
/// association reports `Locked`; a frame held on prediction (a miss, an
/// appearance-ambiguous hold, or the transitional re-confirm out of a coast)
/// reports `Uncertain`; the single frame on which a track is declared lost (its
/// coast budget exhausted, before any re-acquire) reports `Lost`. `assoc` is the
/// association confidence computed for this frame. A downstream consumer never
/// has to guess whether a box was actually seen-and-resolved this frame or only
/// predicted — a silent identity swap cannot hide because the uncertainty is on
/// the wire.
fn reported(track: &Track, bbox: BoundingBox, lock: LockState, assoc: f32) -> Detection {
    Detection {
        bbox,
        class_label: track.class_label.clone(),
        confidence: track.confidence,
        track_id: Some(track.id),
        assoc_confidence: Some(assoc.clamp(0.0, 1.0)),
        lock_state: Some(lock),
        attributes: None,
    }
}

/// Box center in pixel space.
fn center(b: &BoundingBox) -> (f32, f32) {
    (b.x + b.width / 2.0, b.y + b.height / 2.0)
}

/// Intersection-over-union of two boxes (0 when disjoint).
fn iou(a: &BoundingBox, b: &BoundingBox) -> f32 {
    let ax2 = a.x + a.width;
    let ay2 = a.y + a.height;
    let bx2 = b.x + b.width;
    let by2 = b.y + b.height;
    let ix = (ax2.min(bx2) - a.x.max(b.x)).max(0.0);
    let iy = (ay2.min(by2) - a.y.max(b.y)).max(0.0);
    let inter = ix * iy;
    let union = a.width * a.height + b.width * b.height - inter;
    if union <= 0.0 {
        0.0
    } else {
        inter / union
    }
}

/// Total order on f32 for `max_by` / sort (NaN sorts lowest). Avoids a panic
/// path on `partial_cmp().unwrap()`.
fn total_cmp(a: f32, b: f32) -> std::cmp::Ordering {
    a.total_cmp(&b)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- helpers -----------------------------------------------------------

    fn det(x: f32, y: f32, w: f32, h: f32, conf: f32, label: &str) -> Detection {
        Detection {
            bbox: BoundingBox {
                x,
                y,
                width: w,
                height: h,
            },
            class_label: label.into(),
            confidence: conf,
            track_id: None,
            assoc_confidence: None,
            lock_state: None,
            attributes: None,
        }
    }

    /// A simple object: a box of fixed size whose top-left is `(x, y)`.
    fn obj(x: f32, y: f32) -> Detection {
        det(x, y, 40.0, 40.0, 0.9, "object")
    }

    /// A synthetic appearance descriptor distinct per `tag`. Two detections with
    /// the same tag look alike; different tags look different (orthogonal-ish
    /// one-hot-ish vectors). This stands in for a real re-id embedding so
    /// association can be exercised without pixels — a real extractor builds the
    /// descriptor from the bbox pixels at Gate-B.
    fn appearance_for(tag: usize) -> Appearance {
        let mut v = vec![0.0f32; APPEARANCE_DIM];
        // Put most of the energy in one slot keyed by tag, with a small shared
        // baseline so cosine similarity of different tags is low but not exactly
        // zero (more like real, partially-overlapping descriptors).
        v[tag % APPEARANCE_DIM] = 1.0;
        v[(tag * 5 + 3) % APPEARANCE_DIM] += 0.25;
        Appearance::from_features(v)
    }

    /// A candidate carrying the tag-keyed synthetic descriptor.
    fn cand(d: Detection, tag: usize) -> Candidate {
        Candidate::with_appearance(d, appearance_for(tag))
    }

    /// Run a full motion-only sequence and collect, per frame, the reported id.
    fn run(seq: &[Vec<Detection>], cfg: TrackerConfig) -> Vec<Option<u64>> {
        let mut t = SingleObjectTracker::new(cfg);
        seq.iter().map(|frame| t.update(frame).track_id).collect()
    }

    /// Run an appearance-aware sequence (each frame is a list of candidates).
    fn run_app(seq: &[Vec<Candidate>], cfg: TrackerConfig) -> Vec<TrackUpdate> {
        let mut t = SingleObjectTracker::new(cfg);
        seq.iter()
            .map(|frame| t.update_with_appearance(frame))
            .collect()
    }

    fn reported_ids(ids: &[Option<u64>]) -> Vec<u64> {
        let mut v: Vec<u64> = ids.iter().flatten().copied().collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Count the number of times the reported id changes from one *reported*
    /// frame to the next reported frame (ignoring None gaps).
    fn id_swaps(ids: &[Option<u64>]) -> usize {
        let mut last: Option<u64> = None;
        let mut swaps = 0;
        for id in ids.iter().flatten() {
            if let Some(prev) = last {
                if prev != *id {
                    swaps += 1;
                }
            }
            last = Some(*id);
        }
        swaps
    }

    /// Distance from a reported box center to a given object's box center.
    fn center_err(report: &Detection, truth: &Detection) -> f32 {
        let (rx, ry) = center(&report.bbox);
        let (tx, ty) = center(&truth.bbox);
        ((rx - tx).powi(2) + (ry - ty).powi(2)).sqrt()
    }

    // ---- unit tests on the primitives -------------------------------------

    #[test]
    fn iou_basics() {
        let a = BoundingBox {
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
        };
        let b = a;
        assert!((iou(&a, &b) - 1.0).abs() < 1e-6);
        let c = BoundingBox {
            x: 100.0,
            y: 100.0,
            width: 10.0,
            height: 10.0,
        };
        assert_eq!(iou(&a, &c), 0.0);
        let d = BoundingBox {
            x: 5.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
        };
        // overlap area 5*10=50, union 100+100-50=150
        assert!((iou(&a, &d) - (50.0 / 150.0)).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_endpoints() {
        let a = Appearance::from_features(vec![1.0, 0.0, 0.0]);
        let same = Appearance::from_features(vec![2.0, 0.0, 0.0]); // scaled = same dir
        let orth = Appearance::from_features(vec![0.0, 1.0, 0.0]);
        let m = HistogramAppearance;
        assert!((m.similarity(&a, &same) - 1.0).abs() < 1e-6);
        assert!(m.similarity(&a, &orth) < 1e-6);
    }

    #[test]
    fn synthetic_descriptors_separate_target_and_distractor() {
        // The harness's tag-keyed descriptors must read as alike within a tag and
        // unlike across tags, or the appearance tests would be vacuous.
        let m = HistogramAppearance;
        let t0 = appearance_for(0);
        let t0b = appearance_for(0);
        let t1 = appearance_for(1);
        assert!(
            (m.similarity(&t0, &t0b) - 1.0).abs() < 1e-6,
            "same tag ~ identical"
        );
        assert!(
            m.similarity(&t0, &t1) < 0.5,
            "different tags must be clearly dissimilar: got {}",
            m.similarity(&t0, &t1)
        );
    }

    #[test]
    fn kalman_predicts_constant_velocity() {
        let cfg = TrackerConfig::default();
        let mut kf = BoxKalman::new(&obj(0.0, 0.0).bbox, &cfg);
        kf.predict();
        kf.update(&obj(10.0, 0.0).bbox);
        kf.predict();
        kf.update(&obj(20.0, 0.0).bbox);
        let before = center(&kf.bbox()).0;
        kf.predict();
        let after = center(&kf.bbox()).0;
        assert!(
            after > before,
            "predicted center should advance: {before} -> {after}"
        );
    }

    #[test]
    fn velocity_is_clamped_against_a_jitter_spike() {
        // A single huge measurement jump must not inject an unbounded velocity.
        let cfg = TrackerConfig {
            max_velocity: 12.0,
            ..TrackerConfig::default()
        };
        let mut kf = BoxKalman::new(&obj(0.0, 0.0).bbox, &cfg);
        kf.predict();
        // A 500px jump in one step: without the clamp the learned velocity would
        // be near 500; it must be bounded to max_velocity.
        kf.update(&obj(500.0, 0.0).bbox);
        assert!(
            kf.vel[0].abs() <= cfg.max_velocity + 1e-3,
            "velocity must be clamped: {}",
            kf.vel[0]
        );
    }

    #[test]
    fn tentative_not_reported_until_confirmed() {
        let cfg = TrackerConfig::default(); // confirm_hits = 2
        let mut t = SingleObjectTracker::new(cfg);
        let u = t.update(&[obj(0.0, 0.0)]);
        assert_eq!(u.state, TrackState::Tentative);
        assert_eq!(u.track_id, None);
        let u = t.update(&[obj(10.0, 0.0)]);
        assert_eq!(u.state, TrackState::Confirmed);
        assert_eq!(u.track_id, Some(1));
    }

    #[test]
    fn single_spurious_blob_never_claims_an_id() {
        let cfg = TrackerConfig::default();
        let mut t = SingleObjectTracker::new(cfg);
        let u1 = t.update(&[obj(0.0, 0.0)]);
        assert_eq!(u1.track_id, None);
        let u2 = t.update(&[]);
        assert_eq!(u2.state, TrackState::Idle);
        assert_eq!(u2.track_id, None);
    }

    // ---- motion-path synthetic scenarios (no distractor) ------------------

    #[test]
    fn scenario_a_weaving_target_at_speed_zero_swaps() {
        let cfg = TrackerConfig::default();
        let mut seq = Vec::new();
        for f in 0..40u32 {
            let x = f as f32 * 18.0;
            let weave = (f as f32 * 0.6).sin() * 25.0;
            seq.push(vec![obj(x, 100.0 + weave)]);
        }
        let ids = run(&seq, cfg);
        assert_eq!(
            id_swaps(&ids),
            0,
            "weaving target must hold one id: {ids:?}"
        );
        assert_eq!(
            reported_ids(&ids),
            vec![1],
            "exactly one id should ever be reported: {ids:?}"
        );
    }

    #[test]
    fn scenario_b_noise_and_dropped_frames_same_id() {
        let cfg = TrackerConfig::default();
        let jitter = [3.0f32, -2.0, 1.0, -3.0, 2.0, 0.0, -1.0, 3.0, -2.0, 1.0];
        let mut seq = Vec::new();
        for f in 0..40u32 {
            if f % 5 == 4 {
                seq.push(vec![]);
                continue;
            }
            let jx = jitter[(f as usize) % jitter.len()];
            let jy = jitter[(f as usize * 7) % jitter.len()];
            let x = f as f32 * 6.0 + jx;
            let y = 80.0 + jy;
            let w = 40.0 + jx;
            let h = 40.0 + jy;
            seq.push(vec![det(x, y, w, h, 0.85, "object")]);
        }
        let ids = run(&seq, cfg);
        assert_eq!(
            id_swaps(&ids),
            0,
            "jitter + dropped frames must hold one id: {ids:?}"
        );
        assert_eq!(
            reported_ids(&ids),
            vec![1],
            "only one id across noise + drops: {ids:?}"
        );
    }

    #[test]
    fn scenario_c_brief_occlusion_keeps_id() {
        let cfg = TrackerConfig::default();
        let mut seq = Vec::new();
        for f in 0..10u32 {
            seq.push(vec![obj(f as f32 * 10.0, 50.0)]);
        }
        for _ in 0..4 {
            seq.push(vec![]);
        }
        for f in 14..24u32 {
            seq.push(vec![obj(f as f32 * 10.0, 50.0)]);
        }
        let ids = run(&seq, cfg);
        assert_eq!(
            id_swaps(&ids),
            0,
            "brief occlusion must keep the id: {ids:?}"
        );
        assert_eq!(
            reported_ids(&ids),
            vec![1],
            "occlusion + re-appear keeps one id: {ids:?}"
        );
        assert_eq!(ids.last().copied().flatten(), Some(1));
    }

    #[test]
    fn scenario_c2_long_occlusion_reacquires_new_id() {
        let cfg = TrackerConfig::default();
        let mut seq = Vec::new();
        for f in 0..10u32 {
            seq.push(vec![obj(f as f32 * 10.0, 50.0)]);
        }
        for _ in 0..15 {
            seq.push(vec![]);
        }
        for f in 0..10u32 {
            seq.push(vec![obj(f as f32 * 10.0, 50.0)]);
        }
        let ids = run(&seq, cfg);
        assert_eq!(
            reported_ids(&ids),
            vec![1, 2],
            "expected re-acquire: {ids:?}"
        );
        let first = ids.iter().flatten().next().copied();
        let last = ids.iter().flatten().last().copied();
        assert_eq!(first, Some(1));
        assert_eq!(last, Some(2));
    }

    // ---- the FIXED adversarial scenarios ---------------------------------

    /// (a) DIFFERENT-appearance shadowing distractor riding the prediction while
    /// the target maneuvers off it. The lock must be HELD on the target, the
    /// distractor NOT captured, zero swaps — because the appearance gate rejects
    /// the differently-coloured distractor even though it is dead-center in the
    /// motion gate. This is the scenario that FAILED motion-only.
    #[test]
    fn scenario_adversarial_shadowing_distractor_held_on_target() {
        let cfg = TrackerConfig::default();
        let mut t = SingleObjectTracker::new(cfg);

        let jit = [1.0f32, -1.0, 0.5, -0.5, 1.0, 0.0, -1.0, 0.5];
        let j = |f: usize, salt: usize| jit[(f * 7 + salt) % jit.len()];

        // Lock unambiguously on the target (tag 0): 4 solo frames, 8 px/frame.
        for f in 0..4usize {
            t.update_with_appearance(&[cand(
                det(
                    f as f32 * 8.0 + j(f, 0),
                    100.0 + j(f, 1),
                    40.0,
                    40.0,
                    0.90,
                    "object",
                ),
                0,
            )]);
        }
        assert_eq!(
            t.current_id(),
            Some(1),
            "target locked as id 1 before crossing"
        );

        // Crossing: a DIFFERENT-looking distractor (tag 1) rides the prediction;
        // the target (tag 0) veers off in y.
        let mut captured_frame: Option<usize> = None;
        let mut end_following_distractor = false;
        for f in 4usize..16 {
            let pred_x = f as f32 * 8.0;
            let target = cand(
                det(
                    pred_x + j(f, 0),
                    100.0 + (f - 4) as f32 * 6.0 + j(f, 1),
                    40.0,
                    40.0,
                    0.90,
                    "object",
                ),
                0,
            );
            let distractor = cand(
                det(
                    pred_x + j(f, 3),
                    100.0 + j(f, 5),
                    40.0,
                    40.0,
                    0.92,
                    "object",
                ),
                1,
            );
            let u = t.update_with_appearance(&[distractor.clone(), target.clone()]);
            if let Some(rep) = &u.detection {
                let to_target = center_err(rep, &target.detection);
                let to_distr = center_err(rep, &distractor.detection);
                let separated = center_err(&target.detection, &distractor.detection) > 50.0;
                let following_distractor = separated && to_distr + 1.0 < to_target;
                if following_distractor && captured_frame.is_none() {
                    captured_frame = Some(f);
                }
                end_following_distractor = following_distractor;
            }
        }

        assert!(
            captured_frame.is_none() && !end_following_distractor,
            "shadowing distractor must NOT capture the lock: captured at {captured_frame:?}, \
             following distractor at end = {end_following_distractor}"
        );
        assert_eq!(
            t.current_id(),
            Some(1),
            "lock still on the target after the crossing"
        );
    }

    /// (b) IDENTICAL-appearance distractor in the same gate: the tracker MUST go
    /// Uncertain and flag re-designation — it must NOT silently swap. This is the
    /// honest, never-silent backstop for the information-theoretically
    /// unsolvable case. We assert the lock-state is Uncertain and that
    /// re-designation is requested at the ambiguous frame; we do NOT claim the
    /// tracker magically tells two identical objects apart.
    #[test]
    fn scenario_identical_distractor_goes_uncertain_and_flags_redesignation() {
        let cfg = TrackerConfig::default();
        let mut t = SingleObjectTracker::new(cfg);

        // Lock on the target (tag 0), solo, 8 px/frame.
        for f in 0..4usize {
            t.update_with_appearance(&[cand(
                det(f as f32 * 8.0, 100.0, 40.0, 40.0, 0.90, "object"),
                0,
            )]);
        }
        assert_eq!(t.current_id(), Some(1));
        assert!(
            !t.needs_redesignation(),
            "no ambiguity before the identical distractor"
        );

        // An IDENTICAL-looking (tag 0) distractor lands in the gate, right beside
        // the predicted target position — two indistinguishable candidates.
        let pred_x = 4.0 * 8.0;
        let target = cand(det(pred_x, 100.0, 40.0, 40.0, 0.90, "object"), 0);
        let identical = cand(det(pred_x + 8.0, 100.0, 40.0, 40.0, 0.91, "object"), 0);
        let u = t.update_with_appearance(&[identical, target]);

        assert!(
            !u.measured,
            "an ambiguous frame must not count as a resolved measurement"
        );
        assert!(
            u.needs_redesignation && t.needs_redesignation(),
            "two identical-looking candidates must raise re-designation, not silently pick one"
        );
        let rep = u
            .detection
            .expect("an ambiguous hold still reports the predicted box");
        assert_eq!(
            rep.lock_state,
            Some(LockState::Uncertain),
            "the ambiguous frame must report Uncertain on the wire, never Locked"
        );
        // The id is held (no silent swap), but it is explicitly provisional.
        assert_eq!(u.track_id, Some(1));

        // Honest residual: the operator re-designates out of band; the latch
        // clears and the lock can be trusted again.
        t.redesignate();
        assert!(
            !t.needs_redesignation(),
            "operator re-designation clears the latch"
        );
    }

    /// (c) jitter churn: a single continuously-visible object under half-box
    /// oscillating jitter must hold ONE id. This FAILED motion-only (the
    /// unbounded velocity poisoned the prediction). The velocity clamp +
    /// covariance-scaled gate fix it.
    #[test]
    fn scenario_jitter_churn_holds_one_id() {
        let cfg = TrackerConfig::default();
        let jit = [
            0.0f32, 20.0, -18.0, 16.0, -20.0, 12.0, -16.0, 18.0, -16.0, 14.0,
        ];
        let mut seq = Vec::new();
        for f in 0..50u32 {
            let jx = jit[(f as usize) % jit.len()];
            let jy = jit[(f as usize * 3) % jit.len()];
            let x = 100.0 + f as f32 * 2.0 + jx;
            let y = 100.0 + jy;
            seq.push(vec![det(x, y, 40.0, 40.0, 0.8, "object")]);
        }
        let ids = run(&seq, cfg);
        assert_eq!(id_swaps(&ids), 0, "jitter must not swap: {ids:?}");
        assert_eq!(
            reported_ids(&ids),
            vec![1],
            "a single jittering object must hold exactly one id: {ids:?}"
        );
    }

    /// (d) lifecycle still sound: occlusion / coast / re-acquire on the
    /// appearance path too. A brief occlusion keeps the id; the coasted frames
    /// report Uncertain.
    #[test]
    fn scenario_appearance_path_lifecycle_sound() {
        let cfg = TrackerConfig::default();
        let mut seq: Vec<Vec<Candidate>> = Vec::new();
        for f in 0..8u32 {
            seq.push(vec![cand(obj(f as f32 * 10.0, 50.0), 0)]);
        }
        for _ in 0..4 {
            seq.push(vec![]);
        }
        for f in 12..20u32 {
            seq.push(vec![cand(obj(f as f32 * 10.0, 50.0), 0)]);
        }
        let ups = run_app(&seq, cfg);
        let ids: Vec<Option<u64>> = ups.iter().map(|u| u.track_id).collect();
        assert_eq!(
            id_swaps(&ids),
            0,
            "appearance-path occlusion must keep the id: {ids:?}"
        );
        assert_eq!(
            reported_ids(&ids),
            vec![1],
            "one id across the occlusion: {ids:?}"
        );
        // A coasted frame in the gap reports Uncertain.
        let any_uncertain = ups
            .iter()
            .filter_map(|u| u.detection.as_ref())
            .any(|d| d.lock_state == Some(LockState::Uncertain));
        assert!(any_uncertain, "coasted frames must report Uncertain");
    }

    /// Honest residual lock-in: identical appearance is resolved by the operator
    /// backstop, NOT magically. After the ambiguous hold the tracker stays
    /// Uncertain/needs-redesignation until either the distractor leaves (the
    /// target alone re-resolves it) or the operator re-designates. Here the
    /// distractor leaves and a lone target re-locks cleanly.
    #[test]
    fn identical_ambiguity_resolves_when_distractor_leaves() {
        let cfg = TrackerConfig::default();
        let mut t = SingleObjectTracker::new(cfg);
        for f in 0..4usize {
            t.update_with_appearance(&[cand(
                det(f as f32 * 8.0, 100.0, 40.0, 40.0, 0.9, "object"),
                0,
            )]);
        }
        // Ambiguous frame.
        let pred_x = 4.0 * 8.0;
        let u = t.update_with_appearance(&[
            cand(det(pred_x + 8.0, 100.0, 40.0, 40.0, 0.91, "object"), 0),
            cand(det(pred_x, 100.0, 40.0, 40.0, 0.9, "object"), 0),
        ]);
        assert!(u.needs_redesignation, "ambiguous while both present");

        // The distractor leaves; only the target remains, on the predicted path.
        let u =
            t.update_with_appearance(&[cand(det(5.0 * 8.0, 100.0, 40.0, 40.0, 0.9, "object"), 0)]);
        assert!(
            u.measured && !u.needs_redesignation,
            "a lone target re-resolves the lock and clears re-designation"
        );
        assert_eq!(
            u.detection.and_then(|d| d.lock_state),
            Some(LockState::Locked)
        );
    }

    // ---- earlier motion-only distractor coverage, kept --------------------

    #[test]
    fn scenario_d_close_distractor_does_not_steal_lock() {
        let cfg = TrackerConfig::default();
        let mut seq = Vec::new();
        for f in 0..40u32 {
            let tx = f as f32 * 8.0;
            let target = obj(tx, 100.0);
            let dx = 320.0 - f as f32 * 8.0;
            let distractor = det(dx, 105.0, 40.0, 40.0, 0.95, "object");
            seq.push(vec![distractor, target]);
        }
        let ids = run(&seq, cfg);
        assert_eq!(
            id_swaps(&ids),
            0,
            "lock must not jump to the distractor: {ids:?}"
        );
        assert_eq!(
            reported_ids(&ids),
            vec![1],
            "exactly one id despite a close distractor: {ids:?}"
        );
    }

    #[test]
    fn scenario_d2_louder_distractor_never_captures() {
        let cfg = TrackerConfig::default();
        let mut t = SingleObjectTracker::new(cfg);
        t.update(&[obj(0.0, 100.0)]);
        let locked = t.update(&[obj(10.0, 100.0)]).track_id;
        assert_eq!(locked, Some(1));
        let mut ids = Vec::new();
        for f in 2..30u32 {
            let target = det(f as f32 * 10.0, 100.0, 40.0, 40.0, 0.6, "object");
            let distractor = det(f as f32 * 10.0 + 120.0, 100.0, 40.0, 40.0, 0.99, "object");
            ids.push(t.update(&[distractor, target]).track_id);
        }
        assert_eq!(
            id_swaps(&ids),
            0,
            "louder distractor must not steal: {ids:?}"
        );
        assert_eq!(reported_ids(&ids), vec![1]);
    }

    /// The combined end-to-end motion-path assertion (no appearance distractor):
    /// weave → noise+drops → occlusion → parallel-lane distractor, one lock,
    /// ZERO swaps.
    #[test]
    fn scenario_combined_zero_swap_end_to_end() {
        let cfg = TrackerConfig::default();
        let mut seq: Vec<Vec<Detection>> = Vec::new();
        let mut x = 0.0f32;
        for f in 0..20u32 {
            let weave = (f as f32 * 0.5).sin() * 20.0;
            seq.push(vec![obj(x, 120.0 + weave)]);
            x += 12.0;
        }
        let jit = [2.0f32, -2.0, 1.0, -1.0, 3.0];
        for f in 0..15u32 {
            if f == 7 {
                seq.push(vec![]);
                x += 12.0;
                continue;
            }
            let j = jit[(f as usize) % jit.len()];
            seq.push(vec![det(x + j, 120.0 + j, 40.0 + j, 40.0, 0.8, "object")]);
            x += 12.0;
        }
        for _ in 0..3 {
            seq.push(vec![]);
            x += 12.0;
        }
        for f in 0..20u32 {
            let target = obj(x, 120.0);
            let dx = x + 60.0 - f as f32 * 6.0;
            let distractor = det(dx, 124.0, 40.0, 40.0, 0.97, "object");
            seq.push(vec![distractor, target]);
            x += 12.0;
        }
        let ids = run(&seq, cfg);
        assert_eq!(
            id_swaps(&ids),
            0,
            "end-to-end: zero identity swaps required, got ids {ids:?}"
        );
        assert_eq!(
            reported_ids(&ids),
            vec![1],
            "end-to-end: a single id must hold throughout: {ids:?}"
        );
    }

    #[test]
    fn deterministic_repeatable() {
        let cfg = TrackerConfig::default();
        let mut seq = Vec::new();
        for f in 0..30u32 {
            seq.push(vec![obj(f as f32 * 7.0, 60.0)]);
        }
        let a = run(&seq, cfg);
        let b = run(&seq, cfg);
        assert_eq!(a, b);
    }

    #[test]
    fn reported_detection_fills_track_id_on_wire_type() {
        let cfg = TrackerConfig::default();
        let mut t = SingleObjectTracker::new(cfg);
        t.update(&[obj(0.0, 0.0)]);
        let u = t.update(&[obj(10.0, 0.0)]);
        let d: Detection = u.detection.expect("confirmed frame reports a detection");
        assert_eq!(d.track_id, Some(1));
        assert_eq!(d.class_label, "object");
        assert_eq!(d.lock_state, Some(LockState::Locked));
        assert!(d.assoc_confidence.is_some());
    }

    #[test]
    fn class_change_is_not_silently_adopted() {
        // A locked "object" must not silently become a "vehicle": a class change
        // on association keeps the original class and does not raise the
        // reported confidence.
        let cfg = TrackerConfig::default();
        let mut t = SingleObjectTracker::new(cfg);
        t.update_with_appearance(&[cand(obj(0.0, 0.0), 0)]);
        t.update_with_appearance(&[cand(obj(10.0, 0.0), 0)]); // confirmed as "object"
        let u = t.update_with_appearance(&[cand(det(20.0, 0.0, 40.0, 40.0, 0.99, "vehicle"), 0)]);
        let d = u.detection.expect("still reporting");
        assert_eq!(
            d.class_label, "object",
            "class change must not be silently adopted"
        );
    }
}
