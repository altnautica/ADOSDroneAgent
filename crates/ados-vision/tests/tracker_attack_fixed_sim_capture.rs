//! Minimal reproduction of the distinct-appearance shadowing capture: a FIXED
//! (non-morphing) distractor whose appearance is admissible but strictly LESS
//! similar to the locked template than the true target, riding the prediction
//! with a louder detector confidence while the true target maneuvers off. The
//! true target is the better appearance match on EVERY frame, yet the held id is
//! dragged onto the distractor with a clean measured Locked (no Uncertain, no
//! re-designation) once the appearance margin clears the ambiguity guard.

use ados_protocol::framebus::{BoundingBox, Detection, LockState};
use ados_vision::tracker::{
    Appearance, Candidate, SingleObjectTracker, TrackerConfig, APPEARANCE_DIM,
};

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

fn target_app() -> Appearance {
    let mut v = vec![0.0f32; APPEARANCE_DIM];
    v[0] = 1.0;
    Appearance::from_features(v)
}

fn app_at_sim(sim: f32) -> Appearance {
    let s = sim.clamp(0.0, 1.0);
    let ortho = (1.0 - s * s).max(0.0).sqrt();
    let mut v = vec![0.0f32; APPEARANCE_DIM];
    v[0] = s;
    v[1] = ortho;
    Appearance::from_features(v)
}

fn center_err(report: &Detection, truth: &Detection) -> f32 {
    let rb = report
        .bbox
        .as_ref()
        .expect("reported detection carries a bbox");
    let tb = truth.bbox.as_ref().expect("truth detection carries a bbox");
    let rx = rb.x + rb.width / 2.0;
    let ry = rb.y + rb.height / 2.0;
    let tx = tb.x + tb.width / 2.0;
    let ty = tb.y + tb.height / 2.0;
    ((rx - tx).powi(2) + (ry - ty).powi(2)).sqrt()
}

/// Fixed sim 0.7 distractor (clearly less alike than the target's 1.0, margin
/// 0.3 >> ambiguous_margin 0.08, yet >> min_appearance 0.5). The held id must
/// stay on the true (veering) target; if it cleanly reports Locked on the
/// distractor while the target is present and separated, that is a silent swap.
#[test]
fn fixed_admissible_distractor_must_not_silently_capture_held_id() {
    let cfg = TrackerConfig::default();
    let mut t = SingleObjectTracker::new(cfg);
    for f in 0..4usize {
        t.update_with_appearance(&[Candidate::with_appearance(
            det(f as f32 * 8.0, 100.0, 40.0, 40.0, 0.90, "object"),
            target_app(),
        )]);
    }
    let held = t.current_id().expect("locked");

    let mut dropped = false;
    let mut silent: Vec<usize> = Vec::new();
    for k in 0..20usize {
        let f = 4 + k;
        let pred_x = f as f32 * 8.0;
        let target = Candidate::with_appearance(
            det(pred_x, 100.0 + k as f32 * 7.0, 40.0, 40.0, 0.90, "object"),
            target_app(), // appearance 1.0 to the template — the BETTER match
        );
        let distractor = Candidate::with_appearance(
            det(pred_x, 100.0, 40.0, 40.0, 0.99, "object"),
            app_at_sim(0.70), // admissible, but strictly less alike than the target
        );
        let u = t.update_with_appearance(&[distractor.clone(), target.clone()]);
        match u.track_id {
            None => dropped = true,
            Some(id) if id != held => dropped = true,
            Some(_) => {}
        }
        if let Some(rep) = &u.detection {
            let to_t = center_err(rep, &target.detection);
            let to_d = center_err(rep, &distractor.detection);
            let separated = center_err(&target.detection, &distractor.detection) > 50.0;
            let follows = separated && to_d + 1.0 < to_t;
            let clean =
                u.measured && rep.lock_state == Some(LockState::Locked) && !u.needs_redesignation;
            if !dropped && u.track_id == Some(held) && follows && clean {
                silent.push(f);
            }
        }
    }
    assert!(
        silent.is_empty(),
        "RESIDUAL SILENT SWAP: the held id {held} cleanly reported Locked on a strictly-less-\
         similar (sim 0.70 vs target 1.0) prediction-riding distractor while the true target was \
         present and separated, at frames {silent:?}. Appearance gating did NOT hold the lock on \
         the better appearance match — the combined score let motion+confidence outvote a clear \
         appearance preference for the target."
    );
}
