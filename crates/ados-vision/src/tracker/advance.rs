//! Per-frame advance of an existing track: associate, coast, hold, drop.

use ados_protocol::framebus::{BoundingBox, LockState};

use super::scoring::{reported, Decision};
use super::{Candidate, SingleObjectTracker, Track, TrackState, TrackUpdate};

impl SingleObjectTracker {
    /// A track exists: predict, score in-gate candidates by the combined cue,
    /// associate / coast / hold-on-ambiguity, and run the lifecycle.
    pub(super) fn advance(&mut self, mut track: Track, candidates: &[&Candidate]) -> TrackUpdate {
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
                track.kf.update(
                    det.bbox
                        .as_ref()
                        .expect("associated candidate carries a bbox"),
                );
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
                // before it is trusted as Confirmed again, a handover laundered
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
                    // immediately, it never earned an id, so nothing to hold.
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
    /// identical-looking candidates), the latter raises re-designation.
    pub(super) fn hold_or_drop(
        &mut self,
        mut track: Track,
        predicted: BoundingBox,
        prev_assoc: f32,
        ambiguous: bool,
    ) -> TrackUpdate {
        if track.misses > self.cfg.max_coast_frames {
            // Real loss: drop the lock. A future detection re-acquires under a
            // fresh id. Clear the ambiguity latch, there is no lock to confirm.
            self.needs_redesignation = false;
            // Emit one final Lost event on the wire for the track that just
            // dropped: the predicted box carries the OLD id flagged
            // `LockState::Lost` so a downstream consumer sees the loss explicitly
            // (this is where the wire enum's `Lost` is produced). The live held id
            // (`TrackUpdate::track_id`) is cleared, the lock is gone, so a
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
}
