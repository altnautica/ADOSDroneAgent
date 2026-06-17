//! Hard adversarial probe: a same-class distractor with a DISTINCT appearance
//! descriptor calibrated into the danger band (admissible but less alike than the
//! true target), riding the target's predicted box while the true target
//! maneuvers off. The question this file answers is whether the appearance-gated
//! association HOLDS the lock on the true target across the whole danger band, or
//! whether some calibration of the distractor's similarity lets its combined
//! score (boosted by a perfect motion fit + higher detector confidence) capture
//! the lock with NO Uncertain / no confidence drop / no lock-state change — a
//! silent swap.
//!
//! Unlike the shipped suite (which uses a trivially-dissimilar tag-1 distractor),
//! these descriptors are tuned to a target similarity in `0..=1` so the exact
//! threshold behaviour of `decide` is exercised.

use ados_protocol::framebus::{BoundingBox, Detection, LockState};
use ados_vision::tracker::{
    Appearance, Candidate, SingleObjectTracker, TrackState, TrackerConfig, APPEARANCE_DIM,
};

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
    }
}

/// The TARGET descriptor: a clean unit vector in slot 0.
fn target_app() -> Appearance {
    let mut v = vec![0.0f32; APPEARANCE_DIM];
    v[0] = 1.0;
    Appearance::from_features(v)
}

/// A distractor descriptor whose cosine similarity to `target_app()` is exactly
/// `sim` (for `sim` in `0..=1`). We build it as `sim` in slot 0 plus an
/// orthogonal component of magnitude `sqrt(1 - sim^2)` in slot 1; the cosine to
/// the target (a unit vector in slot 0) is then `sim` by construction. This lets
/// us sweep the distractor across the whole appearance band, including the narrow
/// danger window just above `min_appearance`.
fn distractor_app(sim: f32) -> Appearance {
    let s = sim.clamp(0.0, 1.0);
    let ortho = (1.0 - s * s).max(0.0).sqrt();
    let mut v = vec![0.0f32; APPEARANCE_DIM];
    v[0] = s;
    v[1] = ortho;
    Appearance::from_features(v)
}

fn center_err(report: &Detection, truth: &Detection) -> f32 {
    let rx = report.bbox.x + report.bbox.width / 2.0;
    let ry = report.bbox.y + report.bbox.height / 2.0;
    let tx = truth.bbox.x + truth.bbox.width / 2.0;
    let ty = truth.bbox.y + truth.bbox.height / 2.0;
    ((rx - tx).powi(2) + (ry - ty).powi(2)).sqrt()
}

/// Lock the tracker onto the target (descriptor = `target_app`) with solo frames,
/// then run the crossing: a distractor at similarity `sim` rides the prediction
/// (perfect motion fit, higher confidence) while the true target veers off in y.
///
/// Returns a per-frame record of the crossing for the caller to judge.
struct FrameRecord {
    frame: usize,
    /// Center distance from the reported box to the true target.
    to_target: f32,
    /// Center distance from the reported box to the distractor.
    to_distractor: f32,
    /// True once the target and distractor are clearly separated.
    separated: bool,
    lock_state: Option<LockState>,
    assoc_confidence: Option<f32>,
    measured: bool,
    needs_redesignation: bool,
    track_id: Option<u64>,
}

fn run_crossing(sim: f32) -> (Vec<FrameRecord>, u64) {
    let cfg = TrackerConfig::default();
    let mut t = SingleObjectTracker::new(cfg);

    // Lock unambiguously on the target, 4 solo frames @ 8 px/frame.
    for f in 0..4usize {
        t.update_with_appearance(&[Candidate::with_appearance(
            det(f as f32 * 8.0, 100.0, 40.0, 40.0, 0.90, "object"),
            target_app(),
        )]);
    }
    let locked_id = t.current_id().expect("target locked before crossing");

    let mut records = Vec::new();
    for f in 4usize..18 {
        let pred_x = f as f32 * 8.0;
        // The true target maneuvers HARD off the predicted line in y.
        let target = Candidate::with_appearance(
            det(
                pred_x,
                100.0 + (f - 4) as f32 * 8.0, // 8 px/frame vertical veer
                40.0,
                40.0,
                0.90,
                "object",
            ),
            target_app(),
        );
        // The distractor rides the prediction exactly (perfect motion fit) and is
        // LOUDER (higher confidence) — the worst case for outranking the target.
        let distractor = Candidate::with_appearance(
            det(pred_x, 100.0, 40.0, 40.0, 0.99, "object"),
            distractor_app(sim),
        );
        let u = t.update_with_appearance(&[distractor.clone(), target.clone()]);
        if let Some(rep) = &u.detection {
            records.push(FrameRecord {
                frame: f,
                to_target: center_err(rep, &target.detection),
                to_distractor: center_err(rep, &distractor.detection),
                separated: center_err(&target.detection, &distractor.detection) > 50.0,
                lock_state: rep.lock_state,
                assoc_confidence: rep.assoc_confidence,
                measured: u.measured,
                needs_redesignation: u.needs_redesignation,
                track_id: u.track_id,
            });
        } else {
            records.push(FrameRecord {
                frame: f,
                to_target: f32::NAN,
                to_distractor: f32::NAN,
                separated: false,
                lock_state: None,
                assoc_confidence: None,
                measured: u.measured,
                needs_redesignation: u.needs_redesignation,
                track_id: u.track_id,
            });
        }
    }
    (records, locked_id)
}

/// A swap is SILENT if, on a frame where the boxes are clearly separated, the
/// reported box follows the distractor (closer to it than to the target) WHILE
/// the wire still says Locked, the frame counts as a resolved measurement, and no
/// re-designation was raised. That is the cardinal sin: the consumer is told
/// "Locked, measured, confident" while the box is actually on the wrong object.
fn silent_swap_frame(r: &FrameRecord) -> bool {
    let following_distractor =
        r.separated && r.to_distractor.is_finite() && r.to_distractor + 1.0 < r.to_target;
    following_distractor
        && r.measured
        && r.lock_state == Some(LockState::Locked)
        && !r.needs_redesignation
}

/// Whether the reported box is following the distractor at all (regardless of how
/// it was flagged). Even an Uncertain-flagged follow is a behaviour worth
/// reporting, but only a *silent* follow is the cardinal sin.
fn following_distractor(r: &FrameRecord) -> bool {
    r.separated && r.to_distractor.is_finite() && r.to_distractor + 1.0 < r.to_target
}

// ===========================================================================
// The danger-band sweep.
// ===========================================================================

/// A TRUE silent in-lock swap requires the ORIGINAL locked id to be dragged onto
/// the distractor with no honesty signal — NOT an honest drop+re-acquire onto a
/// fresh id after a None gap. This walks the per-frame records keeping the id the
/// lock held before the crossing (`locked_id`) and only flags a frame where:
///   * the box follows the distractor while clearly separated,
///   * the frame is a clean measured Locked with no re-designation, AND
///   * the reported id is STILL `locked_id` with NO intervening None drop.
///
/// Re-acquiring a fresh id (id != locked_id, after a None gap) on whatever object
/// remains in frame is correct re-lock behaviour, not a swap of the held identity.
fn true_inlock_swap_frames(records: &[FrameRecord], locked_id: u64) -> Vec<usize> {
    let mut dropped_since_lock = false;
    let mut out = Vec::new();
    for r in records {
        match r.track_id {
            None => dropped_since_lock = true,
            Some(id) if id != locked_id => dropped_since_lock = true,
            Some(_) => {}
        }
        if !dropped_since_lock && r.track_id == Some(locked_id) && silent_swap_frame(r) {
            out.push(r.frame);
        }
    }
    out
}

/// Sweep the distractor's appearance similarity across the whole band and assert
/// NO calibration drags the ORIGINAL held id silently onto the distractor. This
/// is the core attack: somewhere in `(min_appearance, 1 - ambiguous_margin)` is
/// the window where the distractor is admissible AND clearly the best by combined
/// score AND not close enough to the target to trigger the ambiguity hold —
/// exactly where a naive associator picks it silently while still claiming the
/// locked id.
#[test]
fn danger_band_sweep_never_silent_swaps() {
    let mut offenders: Vec<(f32, usize)> = Vec::new();
    // Step finely through the whole band, with extra density around the gate
    // (0.5) and the upper ambiguity edge (0.92).
    let mut sim = 0.0f32;
    while sim <= 1.0001 {
        let (records, locked_id) = run_crossing(sim);
        for frame in true_inlock_swap_frames(&records, locked_id) {
            offenders.push((sim, frame));
        }
        sim += 0.02;
    }
    assert!(
        offenders.is_empty(),
        "TRUE in-lock SILENT SWAP (original id dragged onto distractor, no honesty signal) \
         detected at (similarity, frame) pairs: {offenders:?}"
    );
}

/// The companion to the sweep: the events the sweep correctly does NOT count must
/// still be honest. Across the whole band, every frame where the reported box
/// follows the distractor must EITHER carry a fresh id reached through a None gap
/// (honest re-acquire) OR be flagged Uncertain / not-measured / redesignation.
/// A clean measured Locked on the distractor under the ORIGINAL id is the only
/// thing forbidden — and is exactly what `true_inlock_swap_frames` catches; this
/// asserts there is no OTHER dishonest follow hiding under id-bookkeeping.
#[test]
fn every_distractor_follow_is_honest_or_a_fresh_reacquire() {
    let mut sim = 0.0f32;
    while sim <= 1.0001 {
        let (records, locked_id) = run_crossing(sim);
        let mut dropped = false;
        for r in &records {
            match r.track_id {
                None => dropped = true,
                Some(id) if id != locked_id => dropped = true,
                Some(_) => {}
            }
            if following_distractor(r) {
                let honest = r.lock_state == Some(LockState::Uncertain)
                    || !r.measured
                    || r.needs_redesignation;
                let fresh_reacquire = dropped && r.track_id != Some(locked_id);
                assert!(
                    honest || fresh_reacquire,
                    "sim={sim} frame {} followed the distractor under the original id \
                     {locked_id:?} with a clean measured Locked and no re-designation \
                     (lock={:?}, measured={}, redesig={}, id={:?})",
                    r.frame,
                    r.lock_state,
                    r.measured,
                    r.needs_redesignation,
                    r.track_id
                );
            }
        }
        sim += 0.02;
    }
}

/// The single most dangerous calibration, isolated and fully instrumented: a
/// distractor whose similarity sits in the danger window — above the 0.5 gate,
/// comfortably below the target's ~1.0, and outside the 0.08 ambiguity margin.
/// The target has veered far off. If the appearance gating is sound the lock is
/// HELD on the target (or honestly held Uncertain), never silently dragged onto
/// the louder, prediction-riding distractor.
#[test]
fn worst_case_admissible_distractor_holds_or_uncertain_never_silent() {
    // 0.6: above min_appearance (0.5), well below target (~1.0), and the
    // best-minus-second margin (~0.4) hugely exceeds ambiguous_margin (0.08), so
    // the ambiguity guard will NOT fire — this is precisely the window where a
    // naive best-combined-score pick would silently capture the distractor.
    let sim = 0.6f32;
    let (records, locked_id) = run_crossing(sim);

    let mut silent = Vec::new();
    let mut followed = Vec::new();
    for r in &records {
        if silent_swap_frame(r) {
            silent.push(r.frame);
        }
        if following_distractor(r) {
            followed.push((
                r.frame,
                r.lock_state,
                r.measured,
                r.needs_redesignation,
                r.assoc_confidence,
            ));
        }
    }

    // Cardinal sin: no silent swap anywhere in the crossing.
    assert!(
        silent.is_empty(),
        "SILENT SWAP onto admissible distractor at frames {silent:?}; \
         following records (frame, lock, measured, redesig, assoc) = {followed:?}"
    );

    // Stronger: the reported box must never follow the distractor at all once the
    // two are separated. The appearance gate (0.6 < ... ) should reject the
    // distractor as the best appearance match relative to the target, so the lock
    // either tracks the (veering) target or honestly coasts — but it must not end
    // up on the distractor.
    assert!(
        followed.is_empty(),
        "reported box followed the distractor (even if flagged): {followed:?}"
    );

    // And the id must be unchanged (held on the true target's track).
    assert_eq!(
        run_crossing(sim).0.last().and_then(|r| r.track_id),
        Some(locked_id),
        "the held id must remain the original target's id, never the distractor's"
    );
}

/// A distractor calibrated JUST above the gate (0.52) is the most insidious: it
/// is barely admissible, so the gate does not cleanly veto it, yet it is nowhere
/// near the target's similarity, so the ambiguity guard does not fire either. If
/// `decide` ranked by combined score and then only gate-checked the winner, this
/// is the value that would slip a silent capture through.
#[test]
fn just_above_gate_distractor_is_not_silently_captured() {
    let sim = 0.52f32;
    let (records, _id) = run_crossing(sim);
    let offenders: Vec<usize> = records
        .iter()
        .filter(|r| silent_swap_frame(r))
        .map(|r| r.frame)
        .collect();
    assert!(
        offenders.is_empty(),
        "barely-admissible distractor silently captured at frames {offenders:?}"
    );
}

/// Cross-check the BEHAVIOUR a sound tracker should exhibit when an admissible-
/// but-distinct distractor rides the prediction while the target leaves: at least
/// one frame must be honestly held (Uncertain / not-measured) OR the box tracks
/// the veering target. If instead every frame reports a clean measured Locked on
/// the distractor's position, that is the silent failure. This asserts the
/// tracker DOES something honest, not that it stays perfectly silent-clean while
/// secretly on the wrong box.
#[test]
fn admissible_distractor_provokes_an_honest_response_not_a_clean_lie() {
    let sim = 0.6f32;
    let (records, _id) = run_crossing(sim);

    // Find frames where the two objects are clearly separated.
    let separated: Vec<&FrameRecord> = records.iter().filter(|r| r.separated).collect();
    assert!(
        !separated.is_empty(),
        "the scenario must actually separate the objects, else it is vacuous"
    );

    for r in &separated {
        // On a separated frame, EITHER the box is on the true target (correct
        // hold/track) OR the frame is honestly flagged (Uncertain / not measured /
        // redesignation). A clean measured-Locked report sitting on the distractor
        // is the exact thing that must never happen.
        let on_target = r.to_target.is_finite() && r.to_target + 1.0 < r.to_distractor;
        let honestly_flagged =
            r.lock_state == Some(LockState::Uncertain) || !r.measured || r.needs_redesignation;
        assert!(
            on_target || honestly_flagged,
            "separated frame {} reported a clean measured Locked while NOT on the target \
             (to_target={}, to_distractor={}, lock={:?}, measured={}, redesig={})",
            r.frame,
            r.to_target,
            r.to_distractor,
            r.lock_state,
            r.measured,
            r.needs_redesignation
        );
    }
}

/// Confirm the scenario is non-vacuous: the distractor really does ride the
/// prediction (so a motion-only tracker WOULD capture it). We assert that with no
/// appearance cue (motion-only path) the same geometry DOES drag the box onto the
/// distractor — proving the appearance path is what saves it, not a lucky
/// geometry that never tempted the tracker in the first place.
#[test]
fn scenario_is_non_vacuous_motion_only_would_capture() {
    let cfg = TrackerConfig::default();
    let mut t = SingleObjectTracker::new(cfg);
    // Motion-only lock.
    for f in 0..4usize {
        t.update(&[det(f as f32 * 8.0, 100.0, 40.0, 40.0, 0.90, "object")]);
    }
    assert_eq!(t.state(), TrackState::Confirmed);

    let mut box_pulled_toward_distractor = false;
    for f in 4usize..18 {
        let pred_x = f as f32 * 8.0;
        let target = det(
            pred_x,
            100.0 + (f - 4) as f32 * 8.0,
            40.0,
            40.0,
            0.90,
            "object",
        );
        let distractor = det(pred_x, 100.0, 40.0, 40.0, 0.99, "object");
        let u = t.update(&[distractor.clone(), target.clone()]);
        if let Some(rep) = &u.detection {
            let to_t = center_err(rep, &target);
            let to_d = center_err(rep, &distractor);
            let separated = center_err(&target, &distractor) > 50.0;
            if separated && to_d + 1.0 < to_t {
                box_pulled_toward_distractor = true;
            }
        }
    }
    assert!(
        box_pulled_toward_distractor,
        "the motion-only path must be tempted by this geometry, else the appearance \
         test proves nothing"
    );
}
