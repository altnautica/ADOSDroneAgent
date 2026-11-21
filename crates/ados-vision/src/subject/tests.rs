//! The estimator's rules: beacon fusion, map-aware prediction, stale and
//! gated sightings, coasting out.

use super::scenario_tests::{Eye, Rng};
use super::*;

const CAMERA: [f64; 3] = [0.0, 0.0, -5.0];

/// An estimator fed `frames` sightings of a walker heading north at
/// 1.4 m/s from (6, 0), 10 Hz, with depth.
fn walking(frames: usize) -> (SubjectEstimator, Eye, Rng) {
    let eye = Eye::with_depth(CAMERA);
    let mut rng = Rng::new(3);
    let mut est = SubjectEstimator::new(4, SubjectConfig::default());
    for k in 0..frames {
        let t = k as f64 * 0.1;
        est.observe(&eye.sight(k, t, [6.0 + 1.4 * t, 0.0, 0.0], 1.8, &mut rng));
    }
    (est, eye, rng)
}

fn ns(t: f64) -> u64 {
    (t * 1e9).round() as u64
}

fn beacon(t: f64, n: f32, e: f32) -> BeaconFix {
    BeaconFix {
        stamp_ns: ns(t),
        ned: [n, e, 0.0],
        horiz_acc_m: 3.0,
    }
}

#[test]
fn a_beacon_never_starts_a_track() {
    let mut est = SubjectEstimator::new(1, SubjectConfig::default());
    assert!(est.cfg.beacon_only);
    assert_eq!(
        est.fuse_beacon(&beacon(0.0, 5.0, 0.0)),
        BeaconOutcome::NoVision
    );
    assert!(est.state("main", &OpenSpace).is_none());
}

#[test]
fn a_beacon_fuses_beside_vision_then_alone_within_its_bounds() {
    let (mut est, ..) = walking(30);
    let off = SubjectEstimator {
        cfg: SubjectConfig {
            beacon_only: false,
            ..est.cfg
        },
        ..est.clone()
    };
    // At 2.9 s the walker is at (10.06, 0).
    assert_eq!(
        est.fuse_beacon(&beacon(2.95, 10.1, 0.5)),
        BeaconOutcome::Fused
    );
    let st = est.state("main", &OpenSpace).unwrap();
    assert!(st.sources.beacon && st.sources.depth && st.sources.box_height);
    // Far from the track: another phone, not the subject.
    assert_eq!(
        est.fuse_beacon(&beacon(3.0, 40.0, 30.0)),
        BeaconOutcome::Gated
    );
    // Vision last fixed 1.53 s ago: on the beacon alone, a poor fix is
    // refused and a good one fuses.
    let poor = BeaconFix {
        horiz_acc_m: 12.0,
        ..beacon(4.43, 12.2, 0.0)
    };
    assert_eq!(est.fuse_beacon(&poor), BeaconOutcome::Inaccurate);
    assert_eq!(
        est.fuse_beacon(&beacon(4.43, 12.2, 0.0)),
        BeaconOutcome::Alone
    );
    // A 2 s gap breaks the chain.
    assert_eq!(
        est.fuse_beacon(&beacon(6.5, 15.1, 0.0)),
        BeaconOutcome::Broken
    );
    assert_eq!(
        est.fuse_beacon(&beacon(1.0, 7.0, 0.0)),
        BeaconOutcome::Stale
    );
    // With beacon-only tracking off, a fix without vision does nothing.
    let mut off = off;
    assert_eq!(
        off.fuse_beacon(&beacon(4.43, 12.2, 0.0)),
        BeaconOutcome::NoVision
    );
}

/// A wall across the walker's path, 2 m thick, from east -3 to 3.
struct Wall;

impl FreeSpace for Wall {
    fn is_free(&self, p: [f32; 3]) -> bool {
        !((12.5..14.5).contains(&p[0]) && (-3.0..3.0).contains(&p[1]))
    }
}

/// Everything round the walker is occupied.
struct Boxed;

impl FreeSpace for Boxed {
    fn is_free(&self, p: [f32; 3]) -> bool {
        p[0] < 10.6
    }
}

#[test]
fn predictions_bend_round_occupied_space() {
    let (est, ..) = walking(30);
    let open = est.state("main", &OpenSpace).unwrap();
    let walled = est.state("main", &Wall).unwrap();
    for p in &walled.predictions {
        assert!(Wall.is_free(p.pos_ned), "{p:?} inside the wall");
    }
    let (o, w) = (open.predictions[4].pos_ned, walled.predictions[4].pos_ned);
    assert!(
        !Wall.is_free(o),
        "the open prediction runs into the wall: {o:?}"
    );
    assert!(w[1].abs() > 1.0, "the walled prediction turns aside: {w:?}");
    // The earlier horizons, short of the wall, are unchanged.
    assert_eq!(open.predictions[0].pos_ned, walled.predictions[0].pos_ned);

    let boxed = est.state("main", &Boxed).unwrap();
    for p in &boxed.predictions {
        assert!(p.pos_ned[0] < 10.6, "{p:?} walked into occupied space");
    }
    // The covariance still grows with the horizon.
    let var = |p: &ados_protocol::vision_subject::SubjectPrediction| p.cov[0] + p.cov[4];
    assert!(var(&boxed.predictions[4]) > var(&boxed.predictions[0]));
}

#[test]
fn a_stale_sighting_is_refused_and_a_wild_one_gated_then_restarts() {
    let (mut est, eye, mut rng) = walking(30);
    let old = eye.sight(0, 1.0, [7.4, 0.0, 0.0], 1.8, &mut rng);
    assert_eq!(est.observe(&old), Outcome::Stale);
    // The same subject suddenly 30 m east: three in a row restart the track.
    let wild = |k: usize, rng: &mut Rng| {
        let t = 3.0 + k as f64 * 0.1;
        eye.sight(30 + k, t, [10.0, 30.0, 0.0], 1.8, rng)
    };
    assert_eq!(est.observe(&wild(0, &mut rng)), Outcome::Gated);
    assert_eq!(est.observe(&wild(1, &mut rng)), Outcome::Gated);
    assert_eq!(est.observe(&wild(2, &mut rng)), Outcome::Restarted);
    let st = est.state("main", &OpenSpace).unwrap();
    assert!((st.pos_ned[1] - 30.0).abs() < 1.5, "{:?}", st.pos_ned);
}

#[test]
fn a_track_coasted_past_its_limit_publishes_nothing() {
    let (mut est, ..) = walking(10);
    est.coast(ns(5.0));
    assert!(est.state("main", &OpenSpace).is_some());
    est.coast(ns(6.5));
    assert!(!est.is_tracking());
    assert!(est.state("main", &OpenSpace).is_none());
}

#[test]
fn the_height_is_learned_from_depth_and_stays_a_persons() {
    let (est, ..) = walking(60);
    let (h, sigma) = est.height_m();
    assert!((h - 1.8).abs() < 0.06, "height {h:.3}");
    assert!(sigma < 0.08, "sigma {sigma:.3}");
    let state = est.state("cam", &OpenSpace).unwrap();
    assert_eq!(state.id, 4);
    assert_eq!(state.camera_id, "cam");
    let probs: f32 = state.mode_probs.iter().sum();
    assert!((probs - 1.0).abs() < 1e-4);
}
