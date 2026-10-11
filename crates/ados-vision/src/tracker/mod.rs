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

mod advance;
mod appearance;
mod config;
mod kalman;
mod scoring;

pub use appearance::{Appearance, AppearanceModel, Candidate, HistogramAppearance, APPEARANCE_DIM};
pub use config::{ScoreWeights, TrackState, TrackUpdate, TrackerConfig};

use ados_protocol::framebus::{Detection, LockState};
use kalman::BoxKalman;
use scoring::{reported, total_cmp};

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
        // Filter out detections below the confidence floor up front, and any
        // box-less percept (a mask/pose/depth-only reading): this is a 2D-box
        // tracker, so a candidate with no `bbox` is not a follow target.
        let eligible: Vec<&Candidate> = candidates
            .iter()
            .filter(|c| {
                c.detection.confidence >= self.cfg.min_confidence && c.detection.bbox.is_some()
            })
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
                    kf: BoxKalman::new(
                        det.bbox
                            .as_ref()
                            .expect("eligible candidate carries a bbox"),
                        &self.cfg,
                    ),
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
}

#[cfg(test)]
mod tests;
