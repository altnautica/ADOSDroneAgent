//! SITL capture gate: drive the real capture loop end to end with a synthetic
//! frame source and a replay pose source, over a real atlas bus, and assert the
//! keyframe + pose + capture-state streams come out correctly — with no camera,
//! no flight controller, and no shared memory.

use std::sync::Arc;
use std::time::Duration;

use ados_atlas::{
    diagonal_cov, mono_ns, run_capture_loop, AtlasFrameSource, AtlasPublisher, AtlasRuntimeConfig,
    CameraConfig, CaptureConfig, CaptureProfile, CaptureSession, CapturedFrame, KeyframeBudget,
    PoseProvider, PoseSample, ReplayPose, SelectionParams, SyntheticFrameSource,
};
use ados_protocol::atlas::{
    AtlasEvent, CameraRole, CaptureStatus, GlobalAnchor, ImageEncoding, KeyframeEnvelope, Pose,
    PoseDescriptor, PoseSource, VioHealth, ATLAS_CAPTURE_STATE_TOPIC, ATLAS_KEYFRAME_TOPIC,
    PLUGIN_ATLAS_POSE_TOPIC,
};
use ados_protocol::frame::PLUGIN_MAX_FRAME;
use ados_protocol::framebus::FrameFormat;
use ados_protocol::ipc::{connect_with_retry, read_length_prefixed};
use tokio::sync::Notify;

/// A pose sample whose ANCHOR is latched, so the capture path has a world frame
/// and will select keyframes, and whose arrival is stamped on the local
/// monotonic clock at construction so the freshness gate reads it as current.
fn pose_at(x: f64, ts_ms: i64) -> PoseSample {
    PoseSample {
        pose: Pose {
            r: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            t: [x, 0.0, 0.0],
            cov: None,
        },
        anchor: Some(GlobalAnchor {
            lat: 12.97,
            lon: 77.59,
            alt_m: 920.0,
            yaw_rad: 0.0,
        }),
        source: PoseSource::LocalVio,
        ts_ms,
        arrival_mono_ns: mono_ns(),
        health: VioHealth::Good,
        cov: diagonal_cov(0.03, 0.01),
    }
}

#[tokio::test]
async fn sitl_capture_emits_keyframes_pose_and_state_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("atlas.sock").to_string_lossy().to_string();

    // The atlas bus, with a subscriber connected before any publish.
    let publisher = AtlasPublisher::bind(&sock).await.unwrap();
    let mut sub = connect_with_retry(&sock, 50, Duration::from_millis(20))
        .await
        .unwrap();
    // Let the accept loop register the subscriber before the loop publishes.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Five RGB frames, spaced 100 ms apart, one camera.
    let frames = AtlasFrameSource::Synthetic(SyntheticFrameSource::solid("front", 8, 8, 5, 0, 100));

    // Poses aligned 1:1 with the frames. Baseline is the last KEYFRAME, so:
    //   f0 @0.0   -> first keyframe (session start)
    //   f1 @0.1   -> under 0.5 m  -> pose only
    //   f2 @0.7   -> 0.7 m        -> keyframe
    //   f3 @0.8   -> 0.1 m from 0.7 -> pose only
    //   f4 @1.5   -> 0.8 m from 0.7 -> keyframe
    let poses = vec![
        pose_at(0.0, 0),
        pose_at(0.1, 100),
        pose_at(0.7, 200),
        pose_at(0.8, 300),
        pose_at(1.5, 400),
    ];
    let pose: Arc<dyn PoseProvider> = Arc::new(ReplayPose::new(poses));

    let config = CaptureConfig {
        cameras: vec![CameraConfig {
            id: "front".into(),
            role: CameraRole::Primary,
            enabled: true,
            reconstruct: true,
        }],
        profile: CaptureProfile::Orbit,
        selection: SelectionParams::default(),
    };
    let runtime = AtlasRuntimeConfig {
        enabled: true,
        capture: config.clone(),
        // This test asserts keyframe production, not pose freshness; a wide
        // budget keeps replay timing out of the assertion. The freshness
        // behaviour has its own test below.
        pose_max_age_ms: 60_000,
        ..AtlasRuntimeConfig::default()
    };
    let session = CaptureSession::new(config);
    let cancel = Arc::new(Notify::new());

    // No control commands in this run; the sender stays alive so the control
    // channel remains open (the loop simply never receives a command).
    let (_control_tx, control_rx) = tokio::sync::mpsc::channel(1);
    let loop_cancel = cancel.clone();
    let handle = tokio::spawn(async move {
        run_capture_loop(
            frames,
            pose,
            publisher,
            session,
            runtime,
            "sess-sitl".into(),
            control_rx,
            loop_cancel,
        )
        .await;
    });

    // Collect events until we have the three expected keyframes (or time out).
    let mut keyframes: Vec<KeyframeEnvelope> = Vec::new();
    let mut poses_seen = 0usize;
    let mut states: Vec<CaptureStatus> = Vec::new();
    let deadline = Duration::from_secs(5);
    let collect = tokio::time::timeout(deadline, async {
        // Read until all three keyframes AND the trailing capture-state snapshot
        // that reports the third are in (the state event is published right after
        // its keyframe, so stopping at the keyframe alone would miss it).
        while keyframes.len() < 3 || !states.iter().any(|s| s.keyframes >= 3) {
            let payload = match read_length_prefixed(&mut sub, PLUGIN_MAX_FRAME, true).await {
                Ok(Some(p)) => p,
                _ => break,
            };
            let ev = AtlasEvent::decode(&payload).expect("atlas event");
            match ev.topic.as_str() {
                ATLAS_KEYFRAME_TOPIC => {
                    keyframes.push(KeyframeEnvelope::from_msgpack(&ev.payload).unwrap())
                }
                PLUGIN_ATLAS_POSE_TOPIC => {
                    let _ = PoseDescriptor::from_msgpack(&ev.payload).unwrap();
                    poses_seen += 1;
                }
                ATLAS_CAPTURE_STATE_TOPIC => {
                    states.push(CaptureStatus::from_msgpack(&ev.payload).unwrap())
                }
                other => panic!("unexpected atlas topic {other}"),
            }
        }
    })
    .await;
    cancel.notify_waiters();
    let _ = handle.await;

    assert!(collect.is_ok(), "timed out collecting atlas events");

    // Three keyframes, in order, only the first marks the session start.
    assert_eq!(keyframes.len(), 3, "exactly three keyframes selected");
    assert_eq!(keyframes[0].kf_id, 0);
    assert!(
        keyframes[0].flags.is_session_start,
        "first keyframe starts the session"
    );
    assert!(!keyframes[1].flags.is_session_start);
    assert!(!keyframes[2].flags.is_session_start);
    for kf in &keyframes {
        assert_eq!(kf.session_id, "sess-sitl");
        assert_eq!(kf.camera_id, "front");
        assert_eq!(kf.camera_role, CameraRole::Primary);
        assert_eq!(kf.pose_source, PoseSource::LocalVio);
        assert_eq!(kf.image.encoding, ImageEncoding::Jpeg);
        // The keyframe carries a real JPEG (SOI marker), proving the encode ran.
        assert_eq!(&kf.image.bytes[..2], &[0xFF, 0xD8], "keyframe is a JPEG");
        // Derived intrinsics: principal point centred, positive focal length.
        assert!(kf.camera.k[0] > 0.0);
    }

    // Every frame contributed a pose to the live stream (>= 3 keyframes' frames;
    // at least the frames we fed produced poses).
    assert!(poses_seen >= 3, "pose stream flowed, got {poses_seen}");
    // Capture state advanced (the keyframe count climbed across snapshots).
    assert!(
        states.iter().any(|s| s.keyframes >= 3),
        "capture state reported the keyframe count"
    );
    assert!(states.iter().all(|s| s.session_id == "sess-sitl"));
}

#[tokio::test]
async fn sitl_capture_recovers_from_a_malformed_keyframe_frame() {
    // A frame whose bytes are too small for its declared dimensions fails the
    // keyframe encode. The loop must not panic or stall: it publishes the pose
    // for the bad frame (the ~10 Hz stream stays whole), leaves the selector
    // baseline unadvanced, and produces the keyframe from the next good frame.
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("atlas.sock").to_string_lossy().to_string();
    let publisher = AtlasPublisher::bind(&sock).await.unwrap();
    let mut sub = connect_with_retry(&sock, 50, Duration::from_millis(20))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let good = |ts: i64| CapturedFrame {
        camera_id: "front".into(),
        ts_ms: ts,
        width: 8,
        height: 8,
        format: FrameFormat::Rgb24,
        bytes: vec![0u8; 8 * 8 * 3],
    };
    let frames = AtlasFrameSource::Synthetic(SyntheticFrameSource::new(vec![
        good(0),
        // Malformed: declared 8x8 RGB but only 4 bytes → encode fails.
        CapturedFrame {
            camera_id: "front".into(),
            ts_ms: 100,
            width: 8,
            height: 8,
            format: FrameFormat::Rgb24,
            bytes: vec![0u8; 4],
        },
        good(200),
    ]));
    // Poses past the 0.5 m threshold so both good frames AND the bad one select.
    let pose: Arc<dyn PoseProvider> = Arc::new(ReplayPose::new(vec![
        pose_at(0.0, 0),
        pose_at(0.7, 100),
        pose_at(0.8, 200),
    ]));
    let config = CaptureConfig {
        cameras: vec![CameraConfig {
            id: "front".into(),
            role: CameraRole::Primary,
            enabled: true,
            reconstruct: true,
        }],
        profile: CaptureProfile::Freeform,
        selection: SelectionParams::default(),
    };
    let runtime = AtlasRuntimeConfig {
        enabled: true,
        capture: config.clone(),
        // This test asserts keyframe production, not pose freshness; a wide
        // budget keeps replay timing out of the assertion. The freshness
        // behaviour has its own test below.
        pose_max_age_ms: 60_000,
        ..AtlasRuntimeConfig::default()
    };
    let session = CaptureSession::new(config);
    let cancel = Arc::new(Notify::new());
    let (_control_tx, control_rx) = tokio::sync::mpsc::channel(1);
    let loop_cancel = cancel.clone();
    let handle = tokio::spawn(async move {
        run_capture_loop(
            frames,
            pose,
            publisher,
            session,
            runtime,
            "sess-mal".into(),
            control_rx,
            loop_cancel,
        )
        .await;
    });

    let mut keyframes = 0usize;
    let mut poses = 0usize;
    let collect = tokio::time::timeout(Duration::from_secs(5), async {
        // Two good frames → two keyframes; the malformed frame contributes only a
        // pose. Reaching two keyframes proves the loop recovered.
        while keyframes < 2 {
            let payload = match read_length_prefixed(&mut sub, PLUGIN_MAX_FRAME, true).await {
                Ok(Some(p)) => p,
                _ => break,
            };
            let ev = AtlasEvent::decode(&payload).unwrap();
            match ev.topic.as_str() {
                ATLAS_KEYFRAME_TOPIC => keyframes += 1,
                PLUGIN_ATLAS_POSE_TOPIC => poses += 1,
                ATLAS_CAPTURE_STATE_TOPIC => {}
                other => panic!("unexpected atlas topic {other}"),
            }
        }
    })
    .await;
    cancel.notify_waiters();
    let _ = handle.await;

    assert!(
        collect.is_ok(),
        "loop did not recover from the malformed frame"
    );
    assert_eq!(keyframes, 2, "the two good frames each produced a keyframe");
    assert!(
        poses >= 3,
        "every frame incl. the malformed one published a pose, got {poses}"
    );
}

#[tokio::test]
async fn disabled_config_capture_validates_to_no_cameras() {
    // The daemon's enable/validate gate: a capture config with no enabled camera
    // is rejected before the loop runs (the daemon exits clean on this).
    let cfg = CaptureConfig {
        cameras: vec![CameraConfig {
            id: "front".into(),
            role: CameraRole::Primary,
            enabled: false,
            reconstruct: false,
        }],
        profile: CaptureProfile::Freeform,
        selection: SelectionParams::default(),
    };
    assert!(cfg.validate().is_err(), "no enabled camera is rejected");
}

/// Build the one-camera capture config the freshness tests drive.
fn one_camera_config() -> CaptureConfig {
    CaptureConfig {
        cameras: vec![CameraConfig {
            id: "front".into(),
            role: CameraRole::Primary,
            enabled: true,
            reconstruct: true,
        }],
        profile: CaptureProfile::Freeform,
        selection: SelectionParams::default(),
    }
}

/// Drive the capture loop over a real bus with `poses` and `pose_max_age_ms`,
/// returning every event the subscriber saw within `window`.
async fn run_and_collect(
    poses: Vec<PoseSample>,
    pose_max_age_ms: i64,
    window: Duration,
) -> (Vec<KeyframeEnvelope>, Vec<CaptureStatus>) {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("atlas.sock").to_string_lossy().to_string();
    let publisher = AtlasPublisher::bind(&sock).await.unwrap();
    let mut sub = connect_with_retry(&sock, 50, Duration::from_millis(20))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let n = poses.len();
    let frames = AtlasFrameSource::Synthetic(SyntheticFrameSource::solid("front", 8, 8, n, 0, 100));
    let pose: Arc<dyn PoseProvider> = Arc::new(ReplayPose::new(poses));
    let config = one_camera_config();
    let runtime = AtlasRuntimeConfig {
        enabled: true,
        capture: config.clone(),
        pose_max_age_ms,
        ..AtlasRuntimeConfig::default()
    };
    let session = CaptureSession::new(config);
    let cancel = Arc::new(Notify::new());
    let (_control_tx, control_rx) = tokio::sync::mpsc::channel(1);
    let loop_cancel = cancel.clone();
    let handle = tokio::spawn(async move {
        run_capture_loop(
            frames,
            pose,
            publisher,
            session,
            runtime,
            "sess-fresh".into(),
            control_rx,
            loop_cancel,
        )
        .await;
    });

    let mut keyframes = Vec::new();
    let mut states = Vec::new();
    let _ = tokio::time::timeout(window, async {
        loop {
            let payload = match read_length_prefixed(&mut sub, PLUGIN_MAX_FRAME, true).await {
                Ok(Some(p)) => p,
                _ => break,
            };
            let ev = AtlasEvent::decode(&payload).expect("atlas event");
            match ev.topic.as_str() {
                ATLAS_KEYFRAME_TOPIC => {
                    keyframes.push(KeyframeEnvelope::from_msgpack(&ev.payload).unwrap())
                }
                ATLAS_CAPTURE_STATE_TOPIC => {
                    states.push(CaptureStatus::from_msgpack(&ev.payload).unwrap())
                }
                PLUGIN_ATLAS_POSE_TOPIC => {}
                other => panic!("unexpected atlas topic {other}"),
            }
        }
    })
    .await;
    cancel.notify_waiters();
    let _ = handle.await;
    (keyframes, states)
}

#[tokio::test]
async fn a_stale_pose_stops_keyframe_production_instead_of_freezing_one_in() {
    // THE P0. When the flight-controller link dies mid-flight the pose cache used
    // to keep returning the last pose forever, and every subsequent frame was
    // tagged with that frozen pose and entered the reconstruction as truth — a
    // whole capture placed at one point in space, with no other symptom.
    //
    // Every pose here arrived 10 s ago on the local monotonic clock against a
    // 500 ms budget, which is exactly the shape a frozen cache holds. Nothing may
    // be selected, and the status must SAY it is lost rather than hold `good`.
    let stale: Vec<PoseSample> = (0..5)
        .map(|i| PoseSample {
            arrival_mono_ns: mono_ns() - 10_000_000_000,
            ..pose_at(i as f64, i * 100)
        })
        .collect();
    let (keyframes, states) = run_and_collect(stale, 500, Duration::from_millis(1200)).await;

    assert!(
        keyframes.is_empty(),
        "a pose past its age budget must be treated as absent, got {} keyframes",
        keyframes.len()
    );
    assert!(
        states.iter().any(|s| s.vio_health == VioHealth::Lost),
        "the capture status must report the pose stream lost, saw {:?}",
        states.iter().map(|s| s.vio_health).collect::<Vec<_>>()
    );

    // Control: the identical run with fresh arrivals DOES produce keyframes, so
    // the assertion above is measuring the freshness gate and not a broken rig.
    let fresh: Vec<PoseSample> = (0..5).map(|i| pose_at(i as f64, i * 100)).collect();
    let (keyframes, _) = run_and_collect(fresh, 60_000, Duration::from_millis(1200)).await;
    assert!(
        !keyframes.is_empty(),
        "the same rig with fresh poses must still capture"
    );
}

#[tokio::test]
async fn no_keyframe_is_captured_before_the_geo_anchor_latches() {
    // Before the first 3D fix a pose translation is [0, 0, alt_rel] at the local
    // origin; after it, ENU metres from the anchor. Capturing across that
    // transition put keyframes at the origin AND keyframes hundreds of metres
    // away into one session, which the reconstructor reads as a teleport.
    let unanchored: Vec<PoseSample> = (0..5)
        .map(|i| PoseSample {
            anchor: None,
            health: VioHealth::Degraded,
            ..pose_at(i as f64, i * 100)
        })
        .collect();
    let (keyframes, states) =
        run_and_collect(unanchored, 60_000, Duration::from_millis(1200)).await;

    assert!(
        keyframes.is_empty(),
        "no keyframe may be selected before the session has a world frame"
    );
    assert!(
        states.iter().all(|s| !s.anchored),
        "the status must report the capture as unanchored so the operator can see why it is idle"
    );
}

#[tokio::test]
async fn every_keyframe_carries_the_inputs_a_reconstructor_needs() {
    // A keyframe with no position-prior sigma destabilises the bundle
    // adjustment, and one with no time alignment cannot be placed against its
    // pose. Both now ride the envelope, and the consumer-side validator agrees.
    let poses: Vec<PoseSample> = (0..5).map(|i| pose_at(i as f64, i * 100)).collect();
    let (keyframes, _) = run_and_collect(poses, 60_000, Duration::from_millis(1200)).await;
    assert!(!keyframes.is_empty(), "the rig captured");

    // The replay provider stamps every pose's arrival up front, so by the time
    // the loop reaches later frames those poses are genuinely tens of ms old.
    // That is exactly what the residual is for, and it is the reason this test
    // validates against an explicit budget instead of the default one: the
    // harness's own pose staleness is real and must not be papered over.
    let harness_budget = KeyframeBudget {
        max_pairing_residual_ms: 5_000.0,
        ..KeyframeBudget::default()
    };
    for kf in &keyframes {
        assert_eq!(
            kf.pose_cov.len(),
            ados_protocol::atlas::POSE_COV_LEN,
            "the reconstructor's position-prior sigma must ride the envelope"
        );
        assert!(kf.global_anchor.is_some(), "the world frame is resolved");
        assert!(
            kf.time.ts_monotonic_ns > 0,
            "the frame carries its own monotonic exposure stamp"
        );
        assert!(
            kf.time.pose_pairing_residual_ms.is_finite() && kf.time.pose_pairing_residual_ms >= 0.0,
            "the frame-to-pose pairing cost is measured, not assumed: {}",
            kf.time.pose_pairing_residual_ms
        );
        assert!(
            kf.validate_with(&harness_budget).is_ok(),
            "a captured keyframe must be usable: {:?}",
            kf.validate_with(&harness_budget)
        );
        // This platform has no PPS-disciplined clock, so the offset uncertainty
        // is honestly reported as unmeasured rather than claimed as exact.
        assert!(
            !kf.clock_is_disciplined(),
            "an undisciplined clock must report itself as such"
        );
        assert_eq!(
            kf.time.trigger_seq, None,
            "no CAMERA_TRIGGER lane is wired, so no exact pairing is claimed"
        );
    }

    // The other direction: the residual actually GATES. A frame whose pose was
    // interpolated 120 ms from its exposure instant is refused under the default
    // budget, which is the whole point of measuring it.
    let mut skewed = keyframes[0].clone();
    skewed.time.pose_pairing_residual_ms = 120.0;
    assert!(
        matches!(
            skewed.validate(),
            Err(ados_protocol::atlas::KeyframeReject::PairingResidual { .. })
        ),
        "a 120 ms frame-to-pose skew must be refused, not reconstructed from"
    );
}
