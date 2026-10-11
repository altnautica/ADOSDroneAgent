//! Candidate scoring, the association decision, and geometry helpers.

use ados_protocol::framebus::{BoundingBox, Detection, LockState};

use super::{Appearance, Candidate, SingleObjectTracker, Track, TrackState};

impl SingleObjectTracker {
    /// The live association gate distance: the base center-distance gate widened
    /// by the filter's position uncertainty. During a coast the covariance
    /// grows, so the gate grows with it; a confidently-tracked object has a
    /// tight gate.
    pub(super) fn max_gate_dist(&self, track: &Track, predicted: &BoundingBox) -> f32 {
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
    pub(super) fn score_in_gate<'a>(
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
            // A box-less percept cannot be scored against the predicted box; the
            // eligible filter already drops these, so this only guards the type.
            let Some(det_bbox) = det.bbox.as_ref() else {
                continue;
            };
            let geo_iou = iou(predicted, det_bbox);
            let (dcx, dcy) = center(det_bbox);
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
    pub(super) fn decide(&self, track: &Track, scored: &[Scored<'_>]) -> Decision {
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
    pub(super) fn template_for(&self, cand: &Candidate) -> Option<Appearance> {
        cand.appearance.clone()
    }

    /// Blend a confident match's descriptor into the locked object's template
    /// (EMA at `template_lr`), so the template tracks slow lighting / pose change
    /// without ever snapping onto a one-frame wrong match.
    pub(super) fn blend_template(&self, track: &mut Track, cand: &Candidate) {
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

    pub(super) fn current_id_for(&self, track: &Track) -> Option<u64> {
        match track.state {
            TrackState::Confirmed | TrackState::Coasting => Some(track.id),
            _ => None,
        }
    }
}

/// A scored, in-gate candidate.
pub(super) struct Scored<'a> {
    pub(super) cand: &'a Candidate,
    /// Combined association score (ranking key).
    pub(super) score: f32,
    /// Appearance similarity to the template (1.0 when appearance is not a cue).
    pub(super) appearance: f32,
    /// Whether appearance was actually a live cue for this candidate.
    pub(super) has_app: bool,
    /// Association confidence to stamp on the wire if this one is chosen.
    pub(super) assoc_confidence: f32,
}

/// The association decision for one frame.
pub(super) enum Decision {
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
pub(super) fn motion_fit(geo_iou: f32, dist: f32, gate: f32) -> f32 {
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
pub(super) fn reported(track: &Track, bbox: BoundingBox, lock: LockState, assoc: f32) -> Detection {
    Detection {
        bbox: Some(bbox),
        class_label: track.class_label.clone(),
        confidence: track.confidence,
        track_id: Some(track.id),
        assoc_confidence: Some(assoc.clamp(0.0, 1.0)),
        lock_state: Some(lock),
        attributes: None,
        mask: None,
        keypoints: None,
        depth: None,
        world_pos: None,
    }
}

/// Box center in pixel space.
pub(super) fn center(b: &BoundingBox) -> (f32, f32) {
    (b.x + b.width / 2.0, b.y + b.height / 2.0)
}

/// Intersection-over-union of two boxes (0 when disjoint).
pub(super) fn iou(a: &BoundingBox, b: &BoundingBox) -> f32 {
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
pub(super) fn total_cmp(a: f32, b: f32) -> std::cmp::Ordering {
    a.total_cmp(&b)
}
