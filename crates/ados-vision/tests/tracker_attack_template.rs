//! Long-game attacks on the appearance TEMPLATE itself.
//!
//! The shipped tracker updates the locked object's appearance template with an
//! EMA (`template_lr` = 0.1) on every confident association, so the template can
//! track slow lighting / pose change. A patient adversary could try to exploit
//! that: ride the prediction with an appearance that is admissible but slightly
//! off, get associated frame after frame, and let the EMA drag the template
//! toward the distractor until it eventually captures. These tests check the
//! template cannot be silently walked off the true target.

use ados_protocol::framebus::{BoundingBox, Detection, LockState};
use ados_vision::tracker::{
    Appearance, Candidate, SingleObjectTracker, TrackerConfig, APPEARANCE_DIM,
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

fn target_app() -> Appearance {
    let mut v = vec![0.0f32; APPEARANCE_DIM];
    v[0] = 1.0;
    Appearance::from_features(v)
}

/// Cosine similarity `sim` to the target via an orthogonal slot-1 component.
fn app_at_sim(sim: f32) -> Appearance {
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

/// Sole-distractor template poisoning: the TRUE target is briefly occluded so the
/// distractor is the ONLY candidate (no ambiguity guard, no competing target),
/// and it feeds an admissible-but-off appearance frame after frame on the
/// predicted path. The EMA could walk the template toward it. Then the true
/// target re-appears off-path: if the template was poisoned, the target now looks
/// "wrong" and is rejected; if it held, the target re-locks.
///
/// The honesty contract: across the poisoning frames the distractor may be
/// associated (it is the only thing in the gate and it is admissible), but the
/// template must not drift so far that the genuine target is then locked out —
/// AND no frame may silently report Locked-measured on the distractor while the
/// true target is simultaneously present and separated (there is no such frame
/// here because the target is occluded, so the real check is the re-lock after).
#[test]
fn sole_distractor_cannot_poison_template_enough_to_lock_out_target() {
    let cfg = TrackerConfig::default();
    let mut t = SingleObjectTracker::new(cfg);

    // Lock the target.
    for f in 0..4usize {
        t.update_with_appearance(&[Candidate::with_appearance(
            det(f as f32 * 8.0, 100.0, 40.0, 40.0, 0.90, "object"),
            target_app(),
        )]);
    }
    let target_id = t.current_id().expect("locked");

    // The distractor (sim 0.55, just admissible) is the SOLE candidate on the
    // predicted path for several frames within the coast budget. The true target
    // is occluded (not present). Drive only as many frames as the coast budget
    // allows so the lock is not dropped for unrelated reasons.
    let n = cfg.max_coast_frames as usize - 1;
    for k in 0..n {
        let f = 4 + k;
        let pred_x = f as f32 * 8.0;
        t.update_with_appearance(&[Candidate::with_appearance(
            det(pred_x, 100.0, 40.0, 40.0, 0.95, "object"),
            app_at_sim(0.55),
        )]);
    }

    // The genuine target re-appears (its true appearance, sim 1.0), on its own
    // continued path. It must still be recognisable — the template must not have
    // been walked onto the distractor.
    let mut relocked_on_target = false;
    for k in 0..6usize {
        let f = 4 + n + k;
        let x = f as f32 * 8.0;
        let target =
            Candidate::with_appearance(det(x, 100.0, 40.0, 40.0, 0.90, "object"), target_app());
        let u = t.update_with_appearance(std::slice::from_ref(&target));
        if let Some(rep) = &u.detection {
            if u.measured
                && rep.lock_state == Some(LockState::Locked)
                && center_err(rep, &target.detection) < 5.0
            {
                relocked_on_target = true;
            }
        }
    }
    assert!(
        relocked_on_target,
        "the true target must remain recognisable; a sole admissible distractor must \
         not poison the template enough to lock the genuine target out"
    );
    // The id is either the held original (template held through the coast) or a
    // fresh re-acquire after an honest drop — never a swap onto the distractor's
    // identity while the target was present. Both are acceptable; what matters is
    // the target is tracked, asserted above. Sanity: a live id exists.
    assert!(
        t.current_id().is_some(),
        "a lock must exist on the re-appeared target (held id {target_id} or a fresh one)"
    );
}

/// Chameleon distractor: it STARTS identical to the target (sim ~1.0) to get
/// adopted, then slowly morphs its appearance away while riding the prediction,
/// trying to drag the template with it via the EMA. The true target is present
/// the whole time, separated. The contract (now upheld): no frame may silently
/// report a clean measured Locked on the distractor under the held id while the
/// true target is present and separated. Early on, while both look near-identical,
/// the tracker is allowed to be honestly Uncertain (the appearance-ambiguity
/// hold); once the chameleon morphs distinct it loses the appearance-first race
/// to the strictly-more-similar true target. The template-poison route is closed
/// because the EMA blend is suppressed under contention.
#[test]
fn chameleon_distractor_never_silently_captures_while_target_present() {
    let cfg = TrackerConfig::default();
    let mut t = SingleObjectTracker::new(cfg);
    for f in 0..4usize {
        t.update_with_appearance(&[Candidate::with_appearance(
            det(f as f32 * 8.0, 100.0, 40.0, 40.0, 0.90, "object"),
            target_app(),
        )]);
    }
    let held_id = t.current_id().expect("locked");

    let mut dropped = false;
    let mut silent_capture: Vec<usize> = Vec::new();
    for k in 0..24usize {
        let f = 4 + k;
        let pred_x = f as f32 * 8.0;
        // Distractor morphs from sim 1.0 down to ~0.4 over the run.
        let sim = (1.0 - k as f32 * 0.025).max(0.4);
        // True target veers off in y, keeps its own appearance.
        let target = Candidate::with_appearance(
            det(pred_x, 100.0 + k as f32 * 7.0, 40.0, 40.0, 0.90, "object"),
            target_app(),
        );
        let distractor = Candidate::with_appearance(
            det(pred_x, 100.0, 40.0, 40.0, 0.99, "object"),
            app_at_sim(sim),
        );
        let u = t.update_with_appearance(&[distractor.clone(), target.clone()]);

        match u.track_id {
            None => dropped = true,
            Some(id) if id != held_id => dropped = true,
            Some(_) => {}
        }
        if let Some(rep) = &u.detection {
            let to_t = center_err(rep, &target.detection);
            let to_d = center_err(rep, &distractor.detection);
            let separated = center_err(&target.detection, &distractor.detection) > 50.0;
            let follows_distractor = separated && to_d + 1.0 < to_t;
            let clean =
                u.measured && rep.lock_state == Some(LockState::Locked) && !u.needs_redesignation;
            // A silent capture: the ORIGINAL id, never dropped, reporting a clean
            // measured Locked while sitting on the distractor and away from the
            // present, separated target.
            if !dropped && u.track_id == Some(held_id) && follows_distractor && clean {
                silent_capture.push(f);
            }
        }
    }
    // The chameleon distractor must never silently capture the held id. Its brief
    // near-identical foothold either holds the lock on the true target or surfaces
    // as an honest Uncertain / re-designation (the appearance-ambiguity hold); the
    // appearance-first association never lets the louder, prediction-riding
    // distractor demote the strictly-more-similar true target, and the EMA blend
    // is suppressed under contention so the template is never walked onto the
    // distractor. No frame may report a clean measured Locked on the distractor
    // under the held id while the true target is present and separated.
    assert!(
        silent_capture.is_empty(),
        "SILENT SWAP: the chameleon distractor captured the held id with a clean measured \
         Locked (no Uncertain, no re-designation) while the true target was present and \
         separated, at frames {silent_capture:?}. The appearance-first association must hold \
         the lock on the strictly-more-similar true target or honestly go Uncertain."
    );
}

/// Direct template-stability probe: even when a distractor IS associated (sole
/// candidate, admissible), the EMA at lr=0.1 cannot move the template across the
/// `min_appearance` gate in a single confident association. This bounds how fast
/// any drift can happen and is the property the lock-out test relies on.
#[test]
fn one_admissible_association_cannot_cross_the_gate_in_one_step() {
    // Template starts at the true target (sim 1.0 to itself). One EMA step toward
    // a sim-0.55 descriptor moves it by lr=0.1 of the way; the resulting template
    // must still score the TRUE target well above min_appearance, i.e. one bad
    // association cannot make the genuine target inadmissible next frame.
    let cfg = TrackerConfig::default();
    let mut t = SingleObjectTracker::new(cfg);
    for f in 0..4usize {
        t.update_with_appearance(&[Candidate::with_appearance(
            det(f as f32 * 8.0, 100.0, 40.0, 40.0, 0.90, "object"),
            target_app(),
        )]);
    }
    // One association onto a sole admissible distractor (sim 0.55).
    let f = 4usize;
    t.update_with_appearance(&[Candidate::with_appearance(
        det(f as f32 * 8.0, 100.0, 40.0, 40.0, 0.95, "object"),
        app_at_sim(0.55),
    )]);
    // Next frame: the genuine target alone, on path. It MUST still be admissible
    // (associated, Locked) — proving one bad EMA step did not lock it out.
    let u = t.update_with_appearance(&[Candidate::with_appearance(
        det(5.0 * 8.0, 100.0, 40.0, 40.0, 0.90, "object"),
        target_app(),
    )]);
    let rep = u
        .detection
        .expect("the genuine target must still be reported");
    assert!(
        u.measured && rep.lock_state == Some(LockState::Locked),
        "one admissible-distractor association must not move the template across the \
         gate; the genuine target must still associate (lock={:?}, measured={})",
        rep.lock_state,
        u.measured
    );
}
