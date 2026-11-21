//! Re-identification of the designated subject on rendered frames: the
//! descriptors come from the painted pixels.

use super::scene_tests::{Frame, Scene, Walker};
use super::*;

const RED: [u8; 3] = [200, 30, 30];
const BLUE: [u8; 3] = [30, 30, 200];
const GREEN: [u8; 3] = [30, 190, 40];
const GREY: [u8; 3] = [150, 150, 150];
const YELLOW: [u8; 3] = [220, 210, 30];
const PURPLE: [u8; 3] = [140, 40, 170];

/// The subject leaves view at 3 s and is gone 2 s (past the track's max age).
fn subject() -> Walker {
    let mut s = Walker::new(100.0, 200.0, 10.0, 40.0, 100.0).colours(RED, BLUE);
    s.hidden.push((3.0, 5.0));
    s
}

/// Three people of the subject's build in other clothes, seen beside it.
fn lookalikes() -> Vec<Walker> {
    vec![
        Walker::new(250.0, 200.0, 10.0, 40.0, 100.0).colours(GREEN, BLUE),
        Walker::new(350.0, 190.0, 10.0, 40.0, 100.0).colours(RED, GREY),
        Walker::new(520.0, 200.0, 10.0, 40.0, 100.0).colours(YELLOW, BLUE),
    ]
}

fn entering_at(x: f32, y: f32, t: f64, top: [u8; 3], bottom: [u8; 3]) -> Walker {
    let mut w = Walker::new(x, y, 10.0, 40.0, 100.0).colours(top, bottom);
    w.hidden.push((0.0, t));
    w
}

fn rendered(walkers: Vec<Walker>) -> Vec<Frame> {
    Scene {
        width: 640.0,
        height: 360.0,
        render: true,
        ..Scene::new(walkers)
    }
    .frames(7.0, 11)
}

/// Per frame, each detection's `(walker, track id, appearance id)`.
type Seen = Vec<Vec<(usize, Option<u32>, Option<u32>)>>;

/// Run the frames, designating walker 0 at 0.5 s. Returns the subject's id
/// and what each frame's detections carried.
fn run_designated(frames: &[Frame]) -> (u32, Seen) {
    let mut mot = MultiObjectTracker::new(MotConfig::default());
    let mut sid = None;
    let mut out = Vec::new();
    for (k, f) in frames.iter().enumerate() {
        mot.update(&f.dets, &f.features, Some(f.t_ns));
        if k == 5 {
            let i = f.truth.iter().position(|&w| w == 0).unwrap();
            sid = Some(mot.designate(f.dets[i].bbox.as_ref().unwrap(), "person"));
        }
        out.push(
            (0..f.dets.len())
                .map(|i| {
                    let o = mot.output(i);
                    (
                        f.truth[i],
                        o.and_then(|o| o.track_id),
                        o.and_then(|o| o.appearance_id),
                    )
                })
                .collect(),
        );
    }
    (sid.unwrap(), out)
}

fn t_of(frames: &[Frame], k: usize) -> f64 {
    frames[k].t_ns as f64 / 1e9
}

#[test]
fn the_designated_subject_is_re_identified_among_lookalikes_after_two_seconds_away() {
    let mut s = subject();
    s.jumps.push((5.0, 450.0, 215.0));
    let mut walkers = vec![s];
    walkers.extend(lookalikes());
    walkers.push(entering_at(300.0, 210.0, 4.0, PURPLE, BLUE));
    let frames = rendered(walkers);
    let (sid, out) = run_designated(&frames);

    let mut appearance = None;
    let mut relocked_at = None;
    for (k, dets) in out.iter().enumerate() {
        let t = t_of(&frames, k);
        for &(walker, id, aid) in dets {
            if walker != 0 {
                assert_ne!(
                    id,
                    Some(sid),
                    "walker {walker} took the subject's id at {t:.1} s"
                );
                continue;
            }
            if (0.6..3.0).contains(&t) {
                assert_eq!(id, Some(sid), "subject before leaving, {t:.1} s");
                appearance = appearance.or(aid);
                assert_eq!(aid, appearance);
            }
            if t >= 5.0 && id == Some(sid) {
                relocked_at.get_or_insert(t);
                assert_eq!(aid, appearance, "the appearance identity survives");
            }
        }
    }
    let at = relocked_at.expect("the subject was re-identified");
    assert!(at <= 5.45, "re-identified at {at:.2} s");
    assert!(appearance.is_some());
    // Once re-identified it keeps the id to the end.
    let last = out.last().unwrap();
    assert!(last.iter().any(|&(w, id, _)| w == 0 && id == Some(sid)));
}

#[test]
fn two_indistinguishable_candidates_are_not_re_locked() {
    // The subject does not come back; two people dressed exactly like it
    // walk in instead.
    let mut walkers = vec![subject()];
    walkers[0].hidden[0].1 = 100.0;
    walkers.extend(lookalikes());
    walkers.push(entering_at(440.0, 215.0, 5.0, RED, BLUE));
    walkers.push(entering_at(170.0, 230.0, 5.0, RED, BLUE));
    let frames = rendered(walkers);
    let (sid, out) = run_designated(&frames);
    for (k, dets) in out.iter().enumerate() {
        for &(walker, id, _) in dets {
            if walker != 0 {
                assert_ne!(
                    id,
                    Some(sid),
                    "walker {walker} at {:.1} s",
                    t_of(&frames, k)
                );
            }
        }
    }
}

#[test]
fn a_subject_with_a_known_double_in_view_is_not_re_locked() {
    // Someone dressed exactly like the subject was in view beside it all
    // along: on the subject's return the model cannot tell them apart, so the
    // returning track keeps its own id.
    let mut s = subject();
    s.jumps.push((5.0, 450.0, 215.0));
    let walkers = vec![
        s,
        Walker::new(250.0, 200.0, 10.0, 40.0, 100.0).colours(RED, BLUE),
        Walker::new(520.0, 200.0, 10.0, 40.0, 100.0).colours(GREEN, BLUE),
    ];
    let frames = rendered(walkers);
    let (sid, out) = run_designated(&frames);
    for (k, dets) in out.iter().enumerate() {
        let t = t_of(&frames, k);
        for &(walker, id, _) in dets {
            if walker != 0 || t >= 5.0 {
                assert_ne!(id, Some(sid), "walker {walker} at {t:.1} s");
            }
        }
    }
}

#[test]
fn descriptors_separate_clothing_and_match_the_same_person() {
    let frames = rendered(vec![
        Walker::new(100.0, 200.0, 0.0, 40.0, 100.0).colours(RED, BLUE),
        Walker::new(300.0, 200.0, 0.0, 60.0, 150.0).colours(RED, BLUE),
        Walker::new(500.0, 200.0, 0.0, 40.0, 100.0).colours(GREEN, BLUE),
    ]);
    let f = &frames[0];
    let d: Vec<_> = f.features.iter().map(|x| x.unwrap()).collect();
    assert!(similarity(&d[0], &d[1]) > 0.95, "same clothes, other size");
    assert!(similarity(&d[0], &d[2]) < 0.85, "other top");
    let norm: f32 = d[0].iter().map(|v| v * v).sum();
    assert!((norm - 1.0).abs() < 1e-3);
}

#[test]
fn the_classifier_learns_to_separate_positives_from_negatives() {
    let mut pos = [0.0; FEAT_DIM];
    pos[0] = 1.0;
    let mut neg = [0.0; FEAT_DIM];
    neg[1] = 1.0;
    let mut c = Classifier::default();
    for _ in 0..30 {
        c.train(&pos, true, 0.5, 1e-3);
        c.train(&neg, false, 0.5, 1e-3);
    }
    assert!(c.score(&pos) > 0.9);
    assert!(c.score(&neg) < 0.1);
}
