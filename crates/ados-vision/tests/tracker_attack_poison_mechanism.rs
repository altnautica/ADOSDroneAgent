//! Pin the (now-closed) template-poison route to its mechanism: the appearance
//! TEMPLATE EMA blend.
//!
//! A FIXED distinct-appearance distractor (sim 0.70) does NOT capture (the clean
//! template keeps the true target as the appearance winner). The historical
//! attack was a distractor that is identical for a brief foothold and THEN holds
//! a fixed distinct appearance: the identical-phase associations once blended the
//! distractor into the template (EMA, lr=0.1) and poisoned it, after which the
//! distractor beat the genuine target against the tracker's OWN drifted template.
//! That route is now closed — association is appearance-first (the louder
//! prediction-riding distractor can never demote the strictly-more-similar true
//! target) and the EMA blend is suppressed under contention (so the foothold
//! associations cannot walk the template onto the distractor). Both the control
//! and the foothold sweep must show no clean capture.

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

/// foothold_frames identical (sim 1.0) frames, then a fixed sim-0.70 distractor,
/// always riding the prediction and louder than the veering true target. Returns
/// the frames where the HELD id cleanly (Locked, measured, no-redesig) followed
/// the distractor while the target was present and separated.
fn run(foothold_frames: usize) -> (Vec<usize>, u64) {
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
    let mut silent = Vec::new();
    for k in 0..24usize {
        let f = 4 + k;
        let pred_x = f as f32 * 8.0;
        let dsim = if k < foothold_frames { 1.0 } else { 0.70 };
        let target = Candidate::with_appearance(
            det(pred_x, 100.0 + k as f32 * 7.0, 40.0, 40.0, 0.90, "object"),
            target_app(),
        );
        let distractor = Candidate::with_appearance(
            det(pred_x, 100.0, 40.0, 40.0, 0.99, "object"),
            app_at_sim(dsim),
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
    (silent, held)
}

/// Zero foothold == the fixed sim-0.70 case: clean, no capture (control).
#[test]
fn zero_foothold_fixed_distractor_does_not_capture() {
    let (silent, _) = run(0);
    assert!(
        silent.is_empty(),
        "control: a fixed sim-0.70 distractor with no identical foothold must not capture; \
         got silent-capture frames {silent:?}"
    );
}

/// A brief identical foothold THEN a fixed distinct appearance must NOT capture
/// the held id — the EMA template-poison route is closed. No length of identical
/// foothold (0..=10 frames) may let the subsequently-distinct (sim 0.70)
/// distractor cleanly capture the held id while the true target is present and
/// separated.
///
/// The fix that makes this hold: (1) association is appearance-first, so the
/// strictly-more-similar true target (sim 1.0 to the original template) is never
/// demoted below the sim-0.70 distractor by the distractor's louder confidence /
/// better motion fit; and (2) the EMA blend is suppressed whenever the
/// association is contended / doubted, so the identical-foothold associations can
/// never walk the template onto the distractor and make the capture
/// self-reinforce. The companion `zero_foothold_fixed_distractor_does_not_capture`
/// control proves the no-foothold case was already clean; this proves a foothold
/// cannot smuggle a capture in through the template.
#[test]
fn brief_identical_foothold_then_distinct_does_not_capture_via_template_poison() {
    let mut capturing: Vec<(usize, Vec<usize>)> = Vec::new();
    for fh in 0..=10usize {
        let (silent, _) = run(fh);
        if !silent.is_empty() {
            capturing.push((fh, silent));
        }
    }
    assert!(
        capturing.is_empty(),
        "TEMPLATE-POISON CAPTURE: a brief identical foothold let a subsequently-distinct \
         (sim 0.70) distractor cleanly capture the held id (Locked/measured/no-redesignation) \
         while the true target was present and separated, at (foothold, frames) = {capturing:?}. \
         Appearance-first association + contention-suppressed template blending must close this."
    );
}
