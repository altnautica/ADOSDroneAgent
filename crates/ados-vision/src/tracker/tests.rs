use super::scoring::{center, iou};
use super::*;
use ados_protocol::framebus::BoundingBox;

// ---- helpers -----------------------------------------------------------

fn det(x: f32, y: f32, w: f32, h: f32, conf: f32, label: &str) -> Detection {
    Detection {
        bbox: Some(BoundingBox {
            x,
            y,
            width: w,
            height: h,
        }),
        class_label: label.into(),
        confidence: conf,
        track_id: None,
        assoc_confidence: None,
        lock_state: None,
        attributes: None,
        mask: None,
        keypoints: None,
        depth: None,
        world_pos: None,
    }
}

/// A simple object: a box of fixed size whose top-left is `(x, y)`.
fn obj(x: f32, y: f32) -> Detection {
    det(x, y, 40.0, 40.0, 0.9, "object")
}

/// A synthetic appearance descriptor distinct per `tag`. Two detections with
/// the same tag look alike; different tags look different (orthogonal-ish
/// one-hot-ish vectors). This stands in for a real re-id embedding so
/// association can be exercised without pixels, a real extractor builds the
/// descriptor from the bbox pixels.
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
    let (rx, ry) = center(report.bbox.as_ref().unwrap());
    let (tx, ty) = center(truth.bbox.as_ref().unwrap());
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
    let mut kf = BoxKalman::new(&obj(0.0, 0.0).bbox.unwrap(), &cfg);
    kf.predict();
    kf.update(&obj(10.0, 0.0).bbox.unwrap());
    kf.predict();
    kf.update(&obj(20.0, 0.0).bbox.unwrap());
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
    let mut kf = BoxKalman::new(&obj(0.0, 0.0).bbox.unwrap(), &cfg);
    kf.predict();
    // A 500px jump in one step: without the clamp the learned velocity would
    // be near 500; it must be bounded to max_velocity.
    kf.update(&obj(500.0, 0.0).bbox.unwrap());
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
/// distractor NOT captured, zero swaps, because the appearance gate rejects
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
/// Uncertain and flag re-designation, it must NOT silently swap. This is the
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
    // the predicted target position, two indistinguishable candidates.
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
    let u = t.update_with_appearance(&[cand(det(5.0 * 8.0, 100.0, 40.0, 40.0, 0.9, "object"), 0)]);
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
