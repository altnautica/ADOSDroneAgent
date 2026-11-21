//! The engine's subject estimate: an operator's designated, locked box lifted
//! into NED with the vehicle's pose, depth read in, coasting, the release
//! record, the beacon, and the cases that publish nothing.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::framebus::{BoundingBox, FrameFormat, TIMEBASE_HOST};
use ados_protocol::ipc::{connect_with_retry, read_length_prefixed};
use ados_protocol::vision_depth_frame::{DepthFit, FRAME_COLS, FRAME_ROWS};
use ados_protocol::vision_subject::{SubjectSources, SUBJECT_MAX_BYTES};

use super::*;
use crate::backend::MockBackend;
use crate::depth::feed::FrameTime;
use crate::depth::geometry::CameraGeometry;
use crate::engine::beacon::tests::{home, line};
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

/// The operator's designation of track 3.
const OPERATOR: Option<Designation> = Some(Designation {
    id: 3,
    origin: DesignationOrigin::Operator,
});

fn engine(with_geometry: bool) -> Arc<VisionEngine> {
    let e = VisionEngine::with_tracker(Box::new(MockBackend), 4, true, TrackerConfig::default());
    if with_geometry {
        e.configure_camera_motion(CameraMotionSetup {
            geometries: HashMap::from([
                ("main".to_string(), GEOMETRY),
                ("side".to_string(), GEOMETRY),
            ]),
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

fn desc_on(camera_id: &str, t_ms: u64) -> FrameDescriptor {
    FrameDescriptor {
        v: ados_protocol::framebus::FRAMEBUS_DESCRIPTOR_VERSION,
        camera_id: camera_id.into(),
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

/// One batch on `main` at `t_ms`.
fn step(
    e: &VisionEngine,
    t_ms: u64,
    dets: &[Detection],
    designation: Option<Designation>,
) -> Option<SubjectState> {
    e.update_subject(&desc_on("main", t_ms), dets, designation, t_ms * 1_000_000)
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

/// The subject's box (track 3), with its lock and track state.
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

fn locked() -> [Detection; 1] {
    [subject(TrackState::Confirmed, LockState::Locked)]
}

fn close(a: [f32; 3], b: [f32; 3], tol: f32) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() <= tol)
}

fn released(id: u32, t_ms: u64) -> SubjectRecord {
    SubjectRecord::Released(SubjectReleased {
        camera_id: "main".into(),
        id,
        stamp_ns: t_ms * 1_000_000,
    })
}

/// The export line of a beacon fix at the subject's feet from `publisher`,
/// placed through the test home.
fn beacon_at_feet(publisher: &str) -> Vec<u8> {
    // Metres per degree at 52 N on the WGS-84 ellipsoid.
    let (m_lat, m_lon) = (111_262.0, 68_677.0);
    let h = home();
    line(
        publisher,
        h.lat_deg + f64::from(FEET[0] - h.local_ned[0]) / m_lat,
        h.lon_deg + f64::from(FEET[1] - h.local_ned[1]) / m_lon,
    )
}

#[test]
fn a_locked_box_is_placed_in_ned_and_published() {
    let e = engine(true);
    let mut rx = e.subscribe_subject();
    let st = step(&e, 1_000, &locked(), OPERATOR).unwrap();
    assert!(close(st.pos_ned, FEET, 0.05), "{:?}", st.pos_ned);
    assert_eq!((st.id, st.fix_age_ms), (3, 0));
    assert!(st.sources.box_height && !st.sources.depth && !st.sources.ground);
    assert_eq!(rx.try_recv().unwrap(), SubjectRecord::State(st));
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
    let st = step(&e, 1_000, &locked(), OPERATOR).unwrap();
    assert!(
        st.sources.box_height && st.sources.depth && st.sources.ground,
        "{:?}",
        st.sources
    );
    assert!(close(st.pos_ned, FEET, 0.15), "{:?}", st.pos_ned);
}

#[test]
fn a_coasting_ambiguous_or_lost_lock_moves_the_estimate_on_without_a_fix() {
    let e = engine(true);
    step(&e, 1_000, &locked(), OPERATOR).unwrap();
    let coasting = [subject(TrackState::Lost, LockState::Locked)];
    assert_eq!(
        step(&e, 1_300, &coasting, OPERATOR).unwrap().fix_age_ms,
        300
    );
    let ambiguous = [subject(TrackState::Confirmed, LockState::Uncertain)];
    assert_eq!(
        step(&e, 1_600, &ambiguous, OPERATOR).unwrap().fix_age_ms,
        600
    );
    // The lock lost the subject; the tracker still holds the designation.
    assert_eq!(step(&e, 1_700, &[], OPERATOR).unwrap().fix_age_ms, 700);
}

#[test]
fn a_plugin_designation_is_tracked_but_never_located() {
    let e = engine(true);
    let mut rx = e.subscribe_subject();
    let plugin = Some(Designation {
        id: 3,
        origin: DesignationOrigin::Plugin,
    });
    assert!(step(&e, 1_000, &locked(), plugin).is_none());
    assert!(step(&e, 1_100, &locked(), plugin).is_none());
    assert!(e.subject_runtime().cameras.is_empty());
    assert!(rx.try_recv().is_err(), "nothing on the topic");
}

#[test]
fn the_end_of_a_designation_is_published_as_a_release() {
    let e = engine(true);
    let mut rx = e.subscribe_subject();
    step(&e, 1_000, &locked(), OPERATOR).unwrap();
    assert!(matches!(rx.try_recv(), Ok(SubjectRecord::State(_))));
    assert!(step(&e, 1_100, &locked(), None).is_none());
    assert_eq!(rx.try_recv().unwrap(), released(3, 1_100));
    assert!(e.subject_runtime().cameras.is_empty());
    // Nothing more to release.
    assert!(step(&e, 1_200, &locked(), None).is_none());
    assert!(rx.try_recv().is_err());
}

#[test]
fn a_new_designation_releases_the_old_subject_and_starts_afresh() {
    let e = engine(true);
    let mut rx = e.subscribe_subject();
    step(&e, 1_000, &locked(), OPERATOR).unwrap();
    let _ = rx.try_recv();
    let mut other = subject(TrackState::Confirmed, LockState::Locked);
    other.track_id = Some(9);
    let nine = Some(Designation {
        id: 9,
        origin: DesignationOrigin::Operator,
    });
    let st = step(&e, 1_100, &[other], nine).unwrap();
    assert_eq!(rx.try_recv().unwrap(), released(3, 1_100));
    assert_eq!(rx.try_recv().unwrap(), SubjectRecord::State(st.clone()));
    assert_eq!((st.id, st.fix_age_ms), (9, 0));
}

#[test]
fn a_subject_lost_past_the_coast_limit_is_released() {
    let e = engine(true);
    let mut rx = e.subscribe_subject();
    step(&e, 1_000, &locked(), OPERATOR).unwrap();
    let _ = rx.try_recv();
    assert!(step(&e, 5_900, &[], OPERATOR).is_some());
    let _ = rx.try_recv();
    assert!(step(&e, 6_100, &[], OPERATOR).is_none());
    assert_eq!(rx.try_recv().unwrap(), released(3, 6_100));
}

#[test]
fn nothing_is_published_without_a_camera_geometry_or_pose() {
    let bare = engine(false);
    assert!(step(&bare, 1_000, &locked(), OPERATOR).is_none());
    // A frame the pose histories do not cover places nothing either.
    let e = engine(true);
    assert!(step(&e, 9_000, &locked(), OPERATOR).is_none());
}

/// The next record a socket reader receives.
async fn read_record(client: &mut tokio::net::UnixStream) -> SubjectRecord {
    let body = tokio::time::timeout(
        Duration::from_secs(5),
        read_length_prefixed(client, SUBJECT_MAX_BYTES, true),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    SubjectRecord::from_msgpack(&body).unwrap()
}

#[tokio::test]
async fn a_reader_that_connects_after_a_release_gets_the_release() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("vision-subject.sock");
    let sock = sock.to_str().unwrap().to_string();
    let e = engine(true);
    let cancel = ados_protocol::shutdown::Shutdown::new();
    let server = {
        let (e, cancel, sock) = (e.clone(), cancel.clone(), sock.clone());
        tokio::spawn(async move { crate::subject_bus::serve(e, &sock, cancel).await })
    };
    let mut early = connect_with_retry(&sock, 50, Duration::from_millis(20))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    step(&e, 1_000, &locked(), OPERATOR).unwrap();
    assert!(matches!(
        read_record(&mut early).await,
        SubjectRecord::State(_)
    ));
    step(&e, 1_100, &locked(), None);
    assert_eq!(read_record(&mut early).await, released(3, 1_100));
    // A reader that arrives now is replayed the release, not the position.
    let mut late = connect_with_retry(&sock, 50, Duration::from_millis(20))
        .await
        .unwrap();
    assert_eq!(read_record(&mut late).await, released(3, 1_100));
    cancel.trigger();
    server.await.unwrap().unwrap();
}

#[test]
fn a_beacon_fix_waits_for_a_frame_not_older_than_it_then_fuses() {
    let e = engine(true);
    // No home yet: the fix cannot be placed and is dropped.
    assert!(!e.apply_beacon_line(&beacon_at_feet(BEACON_PUBLISHER), 1_050));
    e.range_feed().set_home(home());
    // Only the host publishes the beacon.
    assert!(!e.apply_beacon_line(&beacon_at_feet("com.example.spoof"), 1_050));

    step(&e, 1_000, &locked(), OPERATOR).unwrap();
    assert!(e.apply_beacon_line(&beacon_at_feet(BEACON_PUBLISHER), 1_150));
    // The frame captured before the fix arrived does not take it.
    let st = step(&e, 1_100, &locked(), OPERATOR).unwrap();
    assert!(!st.sources.beacon);
    assert_eq!(e.subject_runtime().cameras["main"].beacons.len(), 1);
    // The next frame does, first, and is not refused as older than it.
    let st = step(&e, 1_200, &locked(), OPERATOR).unwrap();
    assert!(
        st.sources.beacon && st.sources.box_height,
        "{:?}",
        st.sources
    );
    assert_eq!(st.fix_age_ms, 0);
    assert!(close(st.pos_ned, FEET, 0.2), "{:?}", st.pos_ned);
    assert!(e.subject_runtime().cameras["main"].beacons.is_empty());
}

#[test]
fn each_camera_takes_every_beacon_fix_on_its_own() {
    let e = engine(true);
    e.range_feed().set_home(home());
    // With no estimate running a fix goes nowhere: a beacon starts nothing.
    assert!(e.apply_beacon_line(&beacon_at_feet(BEACON_PUBLISHER), 900));
    assert!(e.subject_runtime().cameras.is_empty());
    step(&e, 1_000, &locked(), OPERATOR).unwrap();
    e.update_subject(&desc_on("side", 1_000), &locked(), OPERATOR, 1_000_000_000)
        .unwrap();
    assert!(e.apply_beacon_line(&beacon_at_feet(BEACON_PUBLISHER), 1_050));
    let rt = e.subject_runtime();
    assert_eq!(rt.cameras["main"].beacons.len(), 1);
    assert_eq!(rt.cameras["side"].beacons.len(), 1);
}

#[test]
fn the_beacon_keeps_the_estimate_after_vision_loses_the_subject() {
    let e = engine(true);
    e.range_feed().set_home(home());
    step(&e, 1_000, &locked(), OPERATOR).unwrap();
    // The phone moves with the subject while vision sees it: the beacon is
    // associated with this designation.
    for t in (1_100..=1_900).step_by(100) {
        assert!(e.apply_beacon_line(&beacon_at_feet(BEACON_PUBLISHER), t));
        step(&e, t, &locked(), OPERATOR).unwrap();
    }
    // Then the lock loses the subject; the designation holds and the fixes
    // keep coming at 10 Hz.
    let mut last = None;
    for t in (2_000..=4_000).step_by(100) {
        assert!(e.apply_beacon_line(&beacon_at_feet(BEACON_PUBLISHER), t));
        last = step(&e, t, &[], OPERATOR);
        assert!(last.is_some(), "at {t} ms");
    }
    let st = last.unwrap();
    let beacon_only = SubjectSources {
        beacon: true,
        ..Default::default()
    };
    assert_eq!(st.sources, beacon_only);
    assert!(close(st.pos_ned, FEET, 1.0), "{:?}", st.pos_ned);
}
