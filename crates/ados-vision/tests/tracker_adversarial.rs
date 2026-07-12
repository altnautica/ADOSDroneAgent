//! Adversarial probe of the single-object tracker's identity safety.
//!
//! These tests ATTACK the tracker, not confirm the happy path: each one tries
//! to provoke a silent identity swap (one id re-pointing at a different object),
//! a coast-forever that should have been a clean drop, a lock onto noise, or a
//! shadowing distractor capturing the lock. They drive only the public API (the
//! same surface the engine uses).
//!
//! The hard cases need appearance, so they use the appearance-aware path with a
//! synthetic descriptor per candidate: a target and a distractor get DIFFERENT
//! descriptors (as two real objects would), and two identical-looking objects
//! get the SAME descriptor (the information-theoretically unsolvable case the
//! tracker must refuse to resolve silently). A real deployment extracts the
//! descriptor from the bbox pixels (Gate-B); here we supply it directly so
//! association is exercised without pixels.

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
        attributes: None,
    }
}

fn obj(x: f32, y: f32) -> Detection {
    det(x, y, 40.0, 40.0, 0.9, "object")
}

/// A synthetic appearance descriptor keyed by `tag`: same tag ~ identical look,
/// different tags ~ clearly dissimilar. Stands in for a learned re-id embedding.
fn appearance_for(tag: usize) -> Appearance {
    let mut v = vec![0.0f32; APPEARANCE_DIM];
    v[tag % APPEARANCE_DIM] = 1.0;
    v[(tag * 5 + 3) % APPEARANCE_DIM] += 0.25;
    Appearance::from_features(v)
}

fn cand(d: Detection, tag: usize) -> Candidate {
    Candidate::with_appearance(d, appearance_for(tag))
}

/// Per-frame reported id (None when nothing is reported), motion-only path.
fn run(seq: &[Vec<Detection>], cfg: TrackerConfig) -> Vec<Option<u64>> {
    let mut t = SingleObjectTracker::new(cfg);
    seq.iter().map(|f| t.update(f).track_id).collect()
}

fn id_changes(ids: &[Option<u64>]) -> usize {
    let mut last = None;
    let mut n = 0;
    for id in ids.iter().flatten() {
        if let Some(p) = last {
            if p != *id {
                n += 1;
            }
        }
        last = Some(*id);
    }
    n
}

/// True if the id stream ever changes from id A directly to id B with NO None
/// frame in between — a silent in-lock swap.
fn has_inlock_swap(ids: &[Option<u64>]) -> bool {
    let mut prev: Option<u64> = None;
    for id in ids {
        match id {
            Some(cur) => {
                if let Some(p) = prev {
                    if p != *cur {
                        return true;
                    }
                }
                prev = Some(*cur);
            }
            None => prev = None,
        }
    }
    false
}

fn distinct(ids: &[Option<u64>]) -> Vec<u64> {
    let mut v: Vec<u64> = ids.iter().flatten().copied().collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Distance from a reported box center to a truth object's center.
fn center_err(report: &Detection, truth: &Detection) -> f32 {
    let rx = report.bbox.x + report.bbox.width / 2.0;
    let ry = report.bbox.y + report.bbox.height / 2.0;
    let tx = truth.bbox.x + truth.bbox.width / 2.0;
    let ty = truth.bbox.y + truth.bbox.height / 2.0;
    ((rx - tx).powi(2) + (ry - ty).powi(2)).sqrt()
}

// ===========================================================================
// (a) Occlusion JUST longer than the coast budget, then re-appearance.
// ===========================================================================

#[test]
fn occlusion_exactly_at_budget_keeps_id() {
    let cfg = TrackerConfig::default(); // max_coast_frames = 8
    let n = cfg.max_coast_frames as usize; // 8
    let mut seq = Vec::new();
    for f in 0..6u32 {
        seq.push(vec![obj(f as f32 * 10.0, 50.0)]);
    }
    for _ in 0..n {
        seq.push(vec![]);
    }
    for f in 0..6u32 {
        let x = 50.0 + (n as f32 + 1.0 + f as f32) * 10.0;
        seq.push(vec![obj(x, 50.0)]);
    }
    let ids = run(&seq, cfg);
    assert!(!has_inlock_swap(&ids), "no in-lock swap allowed: {ids:?}");
    assert_eq!(
        distinct(&ids),
        vec![1],
        "id must survive an at-budget occlusion: {ids:?}"
    );
    assert_eq!(
        ids.last().copied().flatten(),
        Some(1),
        "lock survived to the end: {ids:?}"
    );
}

#[test]
fn occlusion_one_past_budget_drops_and_reacquires() {
    let cfg = TrackerConfig::default();
    let n = cfg.max_coast_frames as usize + 1; // 9 -> drop
    let mut seq = Vec::new();
    for f in 0..6u32 {
        seq.push(vec![obj(f as f32 * 10.0, 50.0)]);
    }
    for _ in 0..n {
        seq.push(vec![]);
    }
    for f in 0..6u32 {
        let x = 50.0 + (n as f32 + 1.0 + f as f32) * 10.0;
        seq.push(vec![obj(x, 50.0)]);
    }
    let ids = run(&seq, cfg);
    assert!(
        !has_inlock_swap(&ids),
        "drop+reacquire must go through a None gap, never an in-lock swap: {ids:?}"
    );
    assert_eq!(
        distinct(&ids),
        vec![1, 2],
        "past-budget occlusion must drop then mint a new id: {ids:?}"
    );
    let first2 = ids.iter().position(|i| *i == Some(2)).unwrap();
    let last1 = ids.iter().rposition(|i| *i == Some(1)).unwrap();
    assert!(last1 < first2, "id 1 must end before id 2 begins: {ids:?}");
    assert!(
        ids[last1 + 1..first2].iter().any(|i| i.is_none()),
        "a None gap must separate the two ids: {ids:?}"
    );
}

#[test]
fn permanent_occlusion_drops_and_stays_idle() {
    let cfg = TrackerConfig::default();
    let mut t = SingleObjectTracker::new(cfg);
    t.update(&[obj(0.0, 50.0)]);
    t.update(&[obj(10.0, 50.0)]); // confirmed, id 1
    assert_eq!(t.state(), TrackState::Confirmed);
    let mut went_idle_at = None;
    for f in 0..200u32 {
        let u = t.update(&[]);
        if u.state == TrackState::Idle && went_idle_at.is_none() {
            went_idle_at = Some(f);
        }
    }
    assert_eq!(t.state(), TrackState::Idle, "must not coast forever");
    assert_eq!(t.current_id(), None, "no live id after a permanent loss");
    assert_eq!(
        went_idle_at,
        Some(cfg.max_coast_frames),
        "must drop on the frame after the budget is exceeded, not later: went idle at {went_idle_at:?}"
    );
}

/// The identity uncertainty must travel on the wire. A measured frame reports
/// `Locked`; a frame held on prediction reports `Uncertain` with a decayed
/// association confidence.
#[test]
fn coasting_frame_reports_uncertain_on_the_wire() {
    let cfg = TrackerConfig::default();
    let mut t = SingleObjectTracker::new(cfg);
    t.update(&[obj(0.0, 50.0)]);
    let measured = t.update(&[obj(10.0, 50.0)]);
    assert_eq!(t.state(), TrackState::Confirmed);
    let measured_det = measured
        .detection
        .expect("a confirmed measured frame reports a detection");
    assert_eq!(
        measured_det.lock_state,
        Some(LockState::Locked),
        "a measured association must report Locked on the wire"
    );
    let measured_assoc = measured_det
        .assoc_confidence
        .expect("a measured frame carries an association confidence");

    let coasted = t.update(&[]);
    assert_eq!(coasted.state, TrackState::Coasting);
    let coasted_det = coasted
        .detection
        .expect("a coasting frame still reports the predicted box");
    assert_eq!(
        coasted_det.track_id, measured_det.track_id,
        "the coasted box holds the same id (no swap)"
    );
    assert_eq!(
        coasted_det.lock_state,
        Some(LockState::Uncertain),
        "a predicted-only frame must report Uncertain, not Locked"
    );
    let coasted_assoc = coasted_det
        .assoc_confidence
        .expect("a coasting frame carries an association confidence");
    assert!(
        coasted_assoc < measured_assoc,
        "coasting must lower the association confidence ({coasted_assoc} !< {measured_assoc})"
    );
}

// ===========================================================================
// (b) Sustained frame drops.
// ===========================================================================

#[test]
fn sustained_one_in_three_drops_holds_id() {
    let cfg = TrackerConfig::default();
    let mut seq = Vec::new();
    for f in 0..90u32 {
        if f % 3 == 2 {
            seq.push(vec![]);
        } else {
            seq.push(vec![obj(f as f32 * 6.0, 70.0)]);
        }
    }
    let ids = run(&seq, cfg);
    assert!(
        !has_inlock_swap(&ids),
        "1-in-3 drops must not swap: {ids:?}"
    );
    assert_eq!(
        distinct(&ids),
        vec![1],
        "1-in-3 drops must hold one id: {ids:?}"
    );
    assert_eq!(ids.last().copied().flatten(), Some(1));
}

#[test]
fn one_in_three_drops_fast_target_holds_id() {
    let cfg = TrackerConfig::default();
    let mut seq = Vec::new();
    for f in 0..60u32 {
        if f % 3 == 2 {
            seq.push(vec![]);
        } else {
            seq.push(vec![obj(f as f32 * 22.0, 70.0)]);
        }
    }
    let ids = run(&seq, cfg);
    assert!(!has_inlock_swap(&ids), "fast 1-in-3 must not swap: {ids:?}");
    assert_eq!(
        distinct(&ids),
        vec![1],
        "fast 1-in-3 must hold one id: {ids:?}"
    );
}

// ===========================================================================
// (b2) Sparse-but-consistent sighting (2-in-3 drops) — identity-SAFE, but a
// documented robustness limit of the confirm phase.
// ===========================================================================

/// A very sparse detector that sees the object only 1 frame in 3 (two drops
/// between sightings) never produces a FALSE or MIS-identified id. The cardinal
/// sin is a silent identity swap; this proves the sparse pattern commits none: it
/// either confirms a single honest id or reports nothing, but it never re-points
/// an id at a different object and never mints a spurious id on noise. This is the
/// identity-safety contract and it holds.
#[test]
fn sparse_two_in_three_sighting_never_mis_identifies() {
    let cfg = TrackerConfig::default();
    let mut seq = Vec::new();
    for f in 0..60u32 {
        if f % 3 == 0 {
            seq.push(vec![obj(f as f32 * 6.0, 70.0)]);
        } else {
            seq.push(vec![]);
        }
    }
    let ids = run(&seq, cfg);
    // Whatever is reported, it is never an in-lock swap and it is at most one id.
    assert!(
        !has_inlock_swap(&ids),
        "a sparse sighting must never swap identity in-lock: {ids:?}"
    );
    assert!(
        distinct(&ids).len() <= 1,
        "a single sparse object must never produce more than one CONCURRENT identity \
         (a clean re-acquire after a real drop is fine, but there must be no mis-id churn): {ids:?}"
    );
}

/// HONEST RESIDUAL (documented, intentionally ignored): a sparse-but-consistent
/// sighting at this density (1 frame on, 2 off, repeating) never CONFIRMS a lock,
/// so it never reports a stable id. This is by design, not a silent-swap bug: the
/// spurious-blob guard drops a still-Tentative track on its very first miss
/// (`single_spurious_blob_never_claims_an_id` depends on exactly that), and
/// confirmation needs `confirm_hits` CONSECUTIVE associated frames — which a
/// 2-in-3-drops pattern can never accumulate. The behaviour is identity-SAFE
/// (`sparse_two_in_three_sighting_never_mis_identifies` proves no false / mis-id),
/// it is only a robustness limit: a detector this sparse simply will not hold a
/// lock. The remedies live outside the never-silent core — a denser detector, or
/// a deployment that sets `confirm_hits = 1` (accepting that a one-frame blob can
/// then claim an id). We do NOT relax the Tentative-drop guard to make this pass,
/// because that guard is the spurious-blob defence; this test stands as the honest
/// record of the trade-off.
#[test]
#[ignore = "documented robustness limit: a 2-in-3-drops sparse sighting never \
            confirms (confirm needs consecutive hits; Tentative drops on first \
            miss by the spurious-blob guard). Identity-safe (no mis-id), not a \
            silent-swap bug. Remedy is a denser detector or confirm_hits=1, not \
            weakening the guard."]
fn sparse_two_in_three_sighting_does_not_confirm_a_lock() {
    let cfg = TrackerConfig::default();
    let mut seq = Vec::new();
    for f in 0..60u32 {
        if f % 3 == 0 {
            seq.push(vec![obj(f as f32 * 6.0, 70.0)]);
        } else {
            seq.push(vec![]);
        }
    }
    let ids = run(&seq, cfg);
    // This is the documented limitation: no id is ever confirmed/reported.
    assert!(
        ids.iter().all(Option::is_none),
        "documented: a 2-in-3-drops sparse sighting never confirms a lock, so no id \
         is reported. If this fails, the confirm-phase lifecycle changed — revisit \
         whether the spurious-blob guard is still intact: {ids:?}"
    );
}

// ===========================================================================
// (c) Bbox jitter at the association-gate boundary — the FIXED churn case.
// ===========================================================================

/// A single continuously-visible object under large oscillating jitter (about
/// half the box width) must HOLD ONE id. This FAILED before the velocity clamp:
/// a half-box jitter slammed an unbounded velocity into the filter, which then
/// overshot the next prediction, threw the jittered box out of the gate, and
/// churned through drop+reacquire (ids 1..5). With the per-update velocity clamp
/// and the covariance-scaled gate, the lock holds id 1.
#[test]
fn boundary_jitter_holds_one_id_no_churn() {
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
    assert!(
        !has_inlock_swap(&ids),
        "gate-boundary jitter must never swap: {ids:?}"
    );
    assert_eq!(
        distinct(&ids),
        vec![1],
        "a single continuously-visible jittering object must hold exactly ONE id (no churn): {ids:?}"
    );
    assert_eq!(
        id_changes(&ids),
        0,
        "no id change at all under pure jitter: {ids:?}"
    );
}

/// The dangerous version of (c): jitter at the gate boundary while a DIFFERENT-
/// looking distractor hovers just outside the gate. A jitter excursion must not
/// let the distractor slip in and capture the lock — appearance vetoes it.
#[test]
fn boundary_jitter_with_lurking_distractor_no_capture() {
    let cfg = TrackerConfig::default();
    let mut t = SingleObjectTracker::new(cfg);
    t.update_with_appearance(&[cand(obj(100.0, 100.0), 0)]);
    t.update_with_appearance(&[cand(obj(102.0, 100.0), 0)]);
    assert_eq!(t.current_id(), Some(1));

    let jit = [0.0f32, 25.0, -24.0, 22.0, -26.0, 18.0];
    let mut ids = Vec::new();
    for f in 0..40u32 {
        let jx = jit[(f as usize) % jit.len()];
        let base = 102.0 + f as f32 * 2.0;
        let target = cand(det(base + jx, 100.0, 40.0, 40.0, 0.7, "object"), 0);
        // A different-looking high-confidence distractor (tag 1) ~95px off.
        let distractor = cand(det(base + 95.0, 100.0, 40.0, 40.0, 0.99, "object"), 1);
        ids.push(t.update_with_appearance(&[distractor, target]).track_id);
    }
    assert!(
        !has_inlock_swap(&ids),
        "a lurking distractor must never capture via a jitter excursion: {ids:?}"
    );
    assert_eq!(
        distinct(&ids),
        vec![1],
        "distractor must not capture the lock: {ids:?}"
    );
}

// ===========================================================================
// The shadowing distractor — the headline B0 fix.
// ===========================================================================

/// A DIFFERENT-appearance distractor sits exactly on the target's prediction and
/// rides it while the real target maneuvers off. Motion-only this captured the
/// lock silently (track_id stayed 1, box dragged onto the distractor). The
/// appearance gate rejects the differently-coloured distractor, so the lock is
/// HELD on the target and the distractor is never captured.
#[test]
fn shadowing_distractor_does_not_capture_with_appearance() {
    let cfg = TrackerConfig::default();
    let mut t = SingleObjectTracker::new(cfg);
    let jit = [1.0f32, -1.0, 0.5, -0.5, 1.0, 0.0, -1.0, 0.5];
    let j = |f: usize, salt: usize| jit[(f * 7 + salt) % jit.len()];

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
    assert_eq!(t.current_id(), Some(1), "target locked as id 1");

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
        "shadowing distractor captured the lock at {captured_frame:?}; following at end = {end_following_distractor}"
    );
    assert_eq!(
        t.current_id(),
        Some(1),
        "lock still on the target after the crossing"
    );
}

/// The IDENTICAL-appearance case: a same-looking distractor in the same gate is
/// impossible to disambiguate from vision. The tracker must go Uncertain and
/// request re-designation — NEVER a silent swap. This is the honest backstop,
/// not a magic resolution.
#[test]
fn identical_distractor_goes_uncertain_never_silent() {
    let cfg = TrackerConfig::default();
    let mut t = SingleObjectTracker::new(cfg);
    for f in 0..4usize {
        t.update_with_appearance(&[cand(
            det(f as f32 * 8.0, 100.0, 40.0, 40.0, 0.90, "object"),
            0,
        )]);
    }
    assert_eq!(t.current_id(), Some(1));
    assert!(!t.needs_redesignation());

    let pred_x = 4.0 * 8.0;
    let target = cand(det(pred_x, 100.0, 40.0, 40.0, 0.90, "object"), 0);
    let identical = cand(det(pred_x + 8.0, 100.0, 40.0, 40.0, 0.91, "object"), 0); // SAME tag
    let u = t.update_with_appearance(&[identical, target]);

    assert!(
        !u.measured,
        "an ambiguous frame is not a resolved measurement"
    );
    assert!(
        u.needs_redesignation && t.needs_redesignation(),
        "two identical-looking candidates must raise re-designation"
    );
    let rep = u
        .detection
        .expect("an ambiguous hold still reports the predicted box");
    assert_eq!(
        rep.lock_state,
        Some(LockState::Uncertain),
        "ambiguous frame reports Uncertain, never Locked"
    );
    assert_eq!(
        u.track_id,
        Some(1),
        "id held but provisional, never silently swapped"
    );

    // Honest residual: the operator re-designates; the latch clears.
    t.redesignate();
    assert!(!t.needs_redesignation());
}

// ===========================================================================
// Coast-onto-distractor paths.
// ===========================================================================

/// A different-looking object arriving AFTER the coast budget on the old
/// predicted path must mint a FRESH id, never resurrect the dropped one.
#[test]
fn distractor_after_budget_mints_fresh_id() {
    let cfg = TrackerConfig::default();
    let mut t = SingleObjectTracker::new(cfg);
    for f in 0..6u32 {
        t.update_with_appearance(&[cand(obj(f as f32 * 10.0, 100.0), 0)]);
    }
    assert_eq!(t.current_id(), Some(1), "target confirmed as id 1");
    for _ in 0..(cfg.max_coast_frames + 2) {
        t.update_with_appearance(&[]);
    }
    assert_eq!(
        t.current_id(),
        None,
        "lock must have dropped after the budget"
    );

    t.update_with_appearance(&[cand(obj(200.0, 100.0), 1)]);
    let u = t.update_with_appearance(&[cand(obj(210.0, 100.0), 1)]);
    assert_eq!(
        u.track_id,
        Some(2),
        "post-budget object must mint a fresh id, got {:?}",
        u.track_id
    );
}

/// A DIFFERENT-looking object drifting onto the coasting prediction WITHIN the
/// budget must not capture the coasting lock: the appearance gate rejects it, so
/// the coast continues and ultimately drops (the true target never returned).
/// Motion-only this would have re-confirmed onto the wrong object.
#[test]
fn different_looking_object_during_coast_does_not_hijack() {
    let cfg = TrackerConfig::default();
    let mut t = SingleObjectTracker::new(cfg);
    for f in 0..6u32 {
        t.update_with_appearance(&[cand(obj(f as f32 * 10.0, 100.0), 0)]);
    }
    assert_eq!(t.current_id(), Some(1));
    // Coast 2 frames (within budget).
    t.update_with_appearance(&[]);
    t.update_with_appearance(&[]);
    assert_eq!(t.state(), TrackState::Coasting);

    // A DIFFERENT-looking object (tag 1) lands on the coasted prediction. The
    // appearance gate rejects it; the lock does not re-confirm onto it.
    let u = t.update_with_appearance(&[cand(det(80.0, 100.0, 40.0, 40.0, 0.95, "object"), 1)]);
    assert_ne!(
        u.state,
        TrackState::Confirmed,
        "a differently-coloured object must not re-confirm the coasting lock"
    );
    // It keeps coasting on the same id (held, not captured) or eventually drops;
    // it must never report a measured Locked on the distractor.
    if let Some(d) = u.detection {
        assert_eq!(
            d.lock_state,
            Some(LockState::Uncertain),
            "still Uncertain, not Locked onto the wrong object"
        );
    }
}

/// An object far OFF the predicted path during a coast must not capture the
/// coasting lock; the coast continues and drops if the true target never
/// returns (motion gate alone already rejects it).
#[test]
fn off_path_object_during_coast_does_not_capture() {
    let cfg = TrackerConfig::default();
    let mut t = SingleObjectTracker::new(cfg);
    for f in 0..6u32 {
        t.update(&[obj(f as f32 * 12.0, 100.0)]);
    }
    assert_eq!(t.current_id(), Some(1));
    let mut captured_wrong = false;
    for _ in 0..(cfg.max_coast_frames) {
        let u = t.update(&[obj(-400.0, 100.0)]);
        if let Some(id) = u.track_id {
            if id != 1 {
                captured_wrong = true;
            }
        }
    }
    assert!(
        !captured_wrong,
        "an off-path object must not capture the coasting lock"
    );
    let u = t.update(&[obj(-400.0, 100.0)]);
    assert_eq!(
        u.track_id, None,
        "off-path object never re-acquired the dropped track in-lock"
    );
}

/// Combined occlusion + sustained drops + boundary jitter in ONE continuous
/// single-object run (no distractor): the realistic field stress. The identity
/// must hold (id 1) through every brief miss, never an in-lock swap, and (since
/// every gap is within budget) never even a clean drop.
#[test]
fn combined_occlusion_drops_jitter_single_object() {
    let cfg = TrackerConfig::default();
    let mut seq: Vec<Vec<Detection>> = Vec::new();
    let jit = [0.0f32, 14.0, -12.0, 10.0, -14.0, 8.0];
    let mut x = 0.0f32;
    for f in 0..30u32 {
        if f % 4 == 3 {
            seq.push(vec![]);
        } else {
            let j = jit[(f as usize) % jit.len()];
            seq.push(vec![det(x + j, 90.0, 40.0, 40.0, 0.8, "object")]);
        }
        x += 9.0;
    }
    for _ in 0..5 {
        seq.push(vec![]);
        x += 9.0;
    }
    for f in 0..20u32 {
        let j = jit[(f as usize) % jit.len()];
        seq.push(vec![det(x + j, 90.0, 40.0, 40.0, 0.8, "object")]);
        x += 9.0;
    }
    let ids = run(&seq, cfg);
    assert!(
        !has_inlock_swap(&ids),
        "combined stress must never swap in-lock: {ids:?}"
    );
    assert_eq!(
        distinct(&ids),
        vec![1],
        "single object, single id through combined brief stress: {ids:?}"
    );
    assert_eq!(
        ids.last().copied().flatten(),
        Some(1),
        "lock alive at the end: {ids:?}"
    );
    assert_eq!(
        id_changes(&ids),
        0,
        "no id change at all when every gap is within budget: {ids:?}"
    );
}
