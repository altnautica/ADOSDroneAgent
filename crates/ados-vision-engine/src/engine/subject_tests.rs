//! The engine's subject estimate: a designated, locked box lifted into NED
//! with the vehicle's pose, depth read in, coasting, and the cases that
//! publish nothing.

use std::collections::HashMap;
use std::sync::Arc;

use ados_protocol::framebus::{BoundingBox, FrameFormat, TIMEBASE_HOST};
use ados_protocol::vision_depth_frame::{DepthFit, FRAME_COLS, FRAME_ROWS};

use super::*;
use crate::backend::MockBackend;
use crate::depth::feed::FrameTime;
use crate::depth::geometry::CameraGeometry;
use crate::engine::beacon::BEACON_PUBLISHER;
use crate::engine::CameraMotionSetup;
use ados_vision::tracker::TrackerConfig;

const W: u32 = 1280;
const H: u32 = 720;
const GEOMETRY: CameraGeometry = CameraGeometry {
    hfov_deg: 90.0,
    pitch_down_deg: 20.0,
};
/// The vehicle hovers 5 m up, level, facing north; the subject (1.70 m)
/// stands 8 m north and 1 m east.
const VEHICLE: [f32; 3] = [0.0, 0.0, -5.0];
const FEET: [f32; 3] = [8.0, 1.0, 0.0];
const HEIGHT: f32 = 1.7;

fn engine(with_geometry: bool) -> Arc<VisionEngine> {
    let e = VisionEngine::with_tracker(Box::new(MockBackend), 4, true, TrackerConfig::default());
    if with_geometry {
        e.configure_camera_motion(CameraMotionSetup {
            geometries: HashMap::from([("main".to_string(), GEOMETRY)]),
            fc_on_sim_clock: false,
        });
    }
    let feed = e.range_feed();
    for ms in (0..=4_000).step_by(100) {
        feed.push_attitude(ms, Attitude::default(), ms + 10);
        feed.push_position(ms, VEHICLE, ms + 10);
    }
    e
}

fn desc(t_ms: u64) -> FrameDescriptor {
    FrameDescriptor {
        v: ados_protocol::framebus::FRAMEBUS_DESCRIPTOR_VERSION,
        camera_id: "main".into(),
        frame_id: t_ms,
        ts_ns: t_ms * 1_000_000,
        ts_sensor_ns: None,
        exposure_ns: None,
        trigger_seq: None,
        timebase: TIMEBASE_HOST,
        width: W,
        height: H,
        format: FrameFormat::Rgb24,
        seq: t_ms,
        slot: 0,
        shm_name: String::new(),
        byte_len: 0,
        hfov_deg: None,
        mount_pitch_deg: None,
    }
}

/// Where a NED point lands in the image, normalised, and its optical depth.
fn project(p: [f32; 3]) -> (f32, f32, f32) {
    let body = [p[0] - VEHICLE[0], p[1] - VEHICLE[1], p[2] - VEHICLE[2]];
    let (z, u, v) = GEOMETRY
        .projector(W as f32 / H as f32)
        .project(body)
        .unwrap();
    (u, v, z)
}

/// The subject's box, locked by the operator, in `state`.
fn subject(state: TrackState, lock: LockState) -> Detection {
    let (u, top, _) = project([FEET[0], FEET[1], FEET[2] - HEIGHT]);
    let (_, foot, _) = project(FEET);
    let height = (foot - top) * H as f32;
    Detection {
        bbox: Some(BoundingBox {
            x: u * W as f32 - 0.2 * height,
            y: top * H as f32,
            width: 0.4 * height,
            height,
        }),
        class_label: "person".into(),
        confidence: 0.9,
        track_id: Some(3),
        assoc_confidence: Some(0.9),
        lock_state: Some(lock),
        attributes: None,
        mask: None,
        keypoints: None,
        depth: None,
        world_pos: None,
        track_state: Some(state),
        appearance_id: Some(1),
    }
}

fn close(a: [f32; 3], b: [f32; 3], tol: f32) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() <= tol)
}

#[test]
fn a_locked_box_is_placed_in_ned_and_published() {
    let e = engine(true);
    let mut rx = e.subscribe_subject();
    let det = [subject(TrackState::Confirmed, LockState::Locked)];
    let st = e
        .update_subject(&desc(1_000), &det, None, 1_000_000_000)
        .unwrap();
    assert!(close(st.pos_ned, FEET, 0.05), "{:?}", st.pos_ned);
    assert_eq!((st.id, st.fix_age_ms), (3, 0));
    assert!(st.sources.box_height && !st.sources.depth && !st.sources.ground);
    assert_eq!(rx.try_recv().unwrap(), st);
    // The frame's attitude and position come from the flight controller's
    // histories at its exposure.
    assert!(e
        .range_feed()
        .position_at(FrameTime::Host(1_000.0))
        .is_some());
}

#[test]
fn depth_on_the_subject_and_the_rangefinder_ground_fuse() {
    let e = engine(true);
    e.range_feed().set_down(5.0, 1_000);
    // A depth frame holding the subject's hips' depth everywhere.
    let (_, _, z) = project([FEET[0], FEET[1], FEET[2] - 0.35 * HEIGHT]);
    let n = usize::from(FRAME_COLS) * usize::from(FRAME_ROWS);
    let frame = DepthFrame {
        camera_id: "main".into(),
        stamp_ns: 980_000_000,
        w: FRAME_COLS,
        h: FRAME_ROWS,
        fx: 64.0,
        fy: 64.0,
        cx: 64.0,
        cy: 48.0,
        pitch_down_deg: 20.0,
        depth_mm: vec![(z * 1000.0) as u16; n],
        sigma_mm: vec![300; n],
        fit: DepthFit {
            scale: 1.0,
            shift: 0.0,
            inliers: 8,
            residual_m: 0.2,
            held_ms: 0,
        },
    };
    e.subject_runtime().depth.insert("main".into(), frame);
    let det = [subject(TrackState::Confirmed, LockState::Locked)];
    let st = e
        .update_subject(&desc(1_000), &det, None, 1_000_000_000)
        .unwrap();
    assert!(
        st.sources.box_height && st.sources.depth && st.sources.ground,
        "{:?}",
        st.sources
    );
    assert!(close(st.pos_ned, FEET, 0.15), "{:?}", st.pos_ned);
}

#[test]
fn a_coasting_or_ambiguous_lock_moves_the_estimate_on_without_a_fix() {
    let e = engine(true);
    let locked = [subject(TrackState::Confirmed, LockState::Locked)];
    e.update_subject(&desc(1_000), &locked, None, 1_000_000_000)
        .unwrap();
    let coasting = [subject(TrackState::Lost, LockState::Locked)];
    let st = e
        .update_subject(&desc(1_300), &coasting, None, 1_300_000_000)
        .unwrap();
    assert_eq!(st.fix_age_ms, 300);
    let ambiguous = [subject(TrackState::Confirmed, LockState::Uncertain)];
    let st = e
        .update_subject(&desc(1_600), &ambiguous, None, 1_600_000_000)
        .unwrap();
    assert_eq!(st.fix_age_ms, 600);
}

#[test]
fn nothing_is_published_without_a_lock_or_a_camera_geometry() {
    let e = engine(true);
    let det = [subject(TrackState::Confirmed, LockState::Locked)];
    e.update_subject(&desc(1_000), &det, Some(3), 1_000_000_000)
        .unwrap();
    // The lock is gone but the tracker still holds the designation: the
    // estimate coasts on.
    let mut free = subject(TrackState::Confirmed, LockState::Locked);
    free.lock_state = None;
    let st = e
        .update_subject(
            &desc(1_100),
            std::slice::from_ref(&free),
            Some(3),
            1_100_000_000,
        )
        .unwrap();
    assert_eq!((st.id, st.fix_age_ms), (3, 100));
    // The designation is released: the estimate is dropped.
    assert!(e
        .update_subject(&desc(1_200), &[free], None, 1_200_000_000)
        .is_none());
    assert!(e.subject_runtime().estimators.is_empty());

    // A camera without a cameras[] entry has no world geometry.
    let bare = engine(false);
    assert!(bare
        .update_subject(&desc(1_000), &det, None, 1_000_000_000)
        .is_none());
    // A frame the pose histories do not cover places nothing either.
    assert!(e
        .update_subject(&desc(9_000), &det, None, 9_000_000_000)
        .is_none());
}

#[test]
fn a_new_designation_restarts_the_estimate() {
    let e = engine(true);
    let det = [subject(TrackState::Confirmed, LockState::Locked)];
    e.update_subject(&desc(1_000), &det, None, 1_000_000_000)
        .unwrap();
    let mut other = subject(TrackState::Confirmed, LockState::Locked);
    other.track_id = Some(9);
    let st = e
        .update_subject(&desc(1_100), &[other], None, 1_100_000_000)
        .unwrap();
    assert_eq!(st.id, 9);
}

/// The export line of a beacon fix at the subject's feet from `publisher`,
/// placed through the test home.
fn beacon_at_feet(publisher: &str) -> Vec<u8> {
    // Metres per degree at 52 N on the WGS-84 ellipsoid.
    let (m_lat, m_lon) = (111_262.0, 68_677.0);
    let h = crate::engine::beacon::tests::home();
    crate::engine::beacon::tests::line(
        publisher,
        h.lat_deg + f64::from(FEET[0] - h.local_ned[0]) / m_lat,
        h.lon_deg + f64::from(FEET[1] - h.local_ned[1]) / m_lon,
    )
}

#[test]
fn a_beacon_fix_waits_for_a_frame_not_older_than_it_then_fuses() {
    let e = engine(true);
    // No home yet: the fix cannot be placed and is dropped.
    assert!(!e.apply_beacon_line(&beacon_at_feet(BEACON_PUBLISHER), 1_050));
    e.range_feed()
        .set_home(crate::engine::beacon::tests::home());
    // Only the host publishes the beacon.
    assert!(!e.apply_beacon_line(&beacon_at_feet("com.example.spoof"), 1_050));

    let det = [subject(TrackState::Confirmed, LockState::Locked)];
    e.update_subject(&desc(1_000), &det, None, 1_000_000_000)
        .unwrap();
    assert!(e.apply_beacon_line(&beacon_at_feet(BEACON_PUBLISHER), 1_150));
    // The frame captured before the fix arrived does not take it.
    let st = e
        .update_subject(&desc(1_100), &det, None, 1_100_000_000)
        .unwrap();
    assert!(!st.sources.beacon);
    assert_eq!(e.subject_runtime().beacons.len(), 1);
    // The next frame does, first, and is not refused as older than it.
    let st = e
        .update_subject(&desc(1_200), &det, None, 1_200_000_000)
        .unwrap();
    assert!(
        st.sources.beacon && st.sources.box_height,
        "{:?}",
        st.sources
    );
    assert_eq!(st.fix_age_ms, 0);
    assert!(close(st.pos_ned, FEET, 0.2), "{:?}", st.pos_ned);
    assert!(e.subject_runtime().beacons.is_empty());
}

#[test]
fn a_beacon_without_a_designated_subject_never_starts_a_track() {
    let e = engine(true);
    e.range_feed()
        .set_home(crate::engine::beacon::tests::home());
    assert!(e.apply_beacon_line(&beacon_at_feet(BEACON_PUBLISHER), 900));
    // A lock whose box is ambiguous: no sighting, and the beacon alone does
    // not start a track.
    let ambiguous = [subject(TrackState::Confirmed, LockState::Uncertain)];
    assert!(e
        .update_subject(&desc(1_000), &ambiguous, Some(3), 1_000_000_000)
        .is_none());
}

#[test]
fn the_beacon_keeps_the_estimate_after_vision_loses_the_subject() {
    let e = engine(true);
    e.range_feed()
        .set_home(crate::engine::beacon::tests::home());
    let det = [subject(TrackState::Confirmed, LockState::Locked)];
    e.update_subject(&desc(1_000), &det, Some(3), 1_000_000_000)
        .unwrap();
    // The lock is lost; the tracker keeps the designation and the phone's
    // fixes keep coming at 5 Hz.
    let mut last = None;
    for k in 1..=25u64 {
        let t = 1_000 + 100 * k;
        if k.is_multiple_of(2) {
            assert!(e.apply_beacon_line(&beacon_at_feet(BEACON_PUBLISHER), t));
        }
        last = e.update_subject(&desc(t), &[], Some(3), t * 1_000_000);
        assert!(last.is_some(), "at {t} ms");
    }
    let st = last.unwrap();
    let beacon_only = ados_protocol::vision_subject::SubjectSources {
        beacon: true,
        ..Default::default()
    };
    assert_eq!(st.sources, beacon_only);
    assert!(close(st.pos_ned, FEET, 1.0), "{:?}", st.pos_ned);
}
