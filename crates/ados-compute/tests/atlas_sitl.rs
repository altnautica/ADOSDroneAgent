//! Atlas SITL gate harness: the mock-runnable slices of the G0→Gn kill-gate
//! ladder, run in-process end-to-end with no real GPU / camera / RF.
//!
//! These prove the BUILT pipeline pieces COMPOSE — a simulated capture's events
//! travel drone→compute over the real LAN bearer + event router, get ingested
//! into the job queue, and are reconstructed (mock) into an output — which the
//! per-crate unit tests do not exercise together. The real-GPU / real-camera /
//! real-RF criteria of each gate are bench items (the stop boundary, M15); a
//! WFB-relay or cloud lane substitutes the same `AtlasBearer` here.

use std::sync::Arc;

use ados_atlas_transport::{
    atlas_event_router, AtlasBearer, AtlasEvent, BearerKind, BearerLadder, DeltaBroadcaster,
    LanHttpBearer, LoopbackBearer,
};
use ados_compute::{
    AtlasIngest, Cluster, Engine, JobRecord, JobStore, LiveSession, LiveSessionState,
    MockDeltaProducer, MockDetector, MockReconstructor, RerunArchetype, RerunRecording, Scheduler,
};
use ados_protocol::atlas::{
    CameraIntrinsics, CameraRole, CaptureState, CaptureStatus, Distortion, ImageEncoding,
    ImuSample, KeyframeEnvelope, KeyframeFlags, KeyframeImage, KeyframeTier, Pose, PoseSource,
    SplatDescriptor, VioHealth, ATLAS_CAPTURE_STATE_TOPIC, ATLAS_KEYFRAME_TOPIC,
    PLUGIN_ATLAS_SPLAT_TOPIC,
};
use ados_protocol::compute::{ComputeJobKind, ComputeJobState, ComputeRole, SlaveDescriptor};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};

fn engine() -> Engine {
    let store = JobStore::open(":memory:").unwrap();
    let scheduler = Scheduler::new(store, Arc::new(MockReconstructor), Arc::new(MockDetector));
    Engine::new(scheduler, Cluster::new_master("compute-sitl"), 1)
}

fn keyframe(i: usize) -> AtlasEvent {
    AtlasEvent {
        topic: ATLAS_KEYFRAME_TOPIC.into(),
        payload: vec![i as u8; 64],
    }
}

fn bagged(keyframes: u64) -> AtlasEvent {
    let status = CaptureStatus {
        session_id: "g0".into(),
        state: CaptureState::Bagged,
        keyframes,
        vio_health: VioHealth::Good,
        camera_count: 1,
        ingest_rate_hz: 9.0,
    };
    AtlasEvent {
        topic: ATLAS_CAPTURE_STATE_TOPIC.into(),
        payload: status.to_msgpack().unwrap(),
    }
}

/// G0: a simulated single-camera capture flows drone→compute over the LAN bearer,
/// is ingested into a dataset, reconstructed (mock), and yields a usable splat.
#[tokio::test]
async fn g0_single_camera_capture_reconstructs_to_a_splat_end_to_end() {
    let engine = engine();

    // The compute node's event receiver on an ephemeral port.
    let (tx, mut rx) = mpsc::channel(64);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, atlas_event_router(tx)).await;
    });

    // The drone forwards keyframes + a bagged state over the LAN bearer.
    let bearer = LanHttpBearer::new(format!("http://{addr}"));
    const N: usize = 5;
    for i in 0..N {
        bearer.send(&keyframe(i)).await.unwrap();
    }
    bearer.send(&bagged(N as u64)).await.unwrap();

    // The compute node ingests each received event; the bagged state submits a job.
    let mut ingest = AtlasIngest::new();
    let mut job_id = None;
    for _ in 0..(N + 1) {
        let ev = rx.recv().await.expect("the bearer delivered the event");
        if let Some(id) = ingest.ingest(&ev, engine.scheduler().store(), 200).unwrap() {
            job_id = Some(id);
        }
    }
    let job_id = job_id.expect("the bagged session submitted a reconstruct job");
    assert_eq!(
        ingest.keyframes_seen(),
        N as u64,
        "every keyframe reached the node"
    );

    // The reconstruct job runs (mock) and yields a splat.
    let outcome = engine.tick(300).unwrap().expect("a job was claimed + run");
    assert_eq!(outcome.job_id, job_id);
    assert_eq!(outcome.state, ComputeJobState::Completed);
    assert!(
        outcome.outputs.iter().any(|o| o.kind == "splat"),
        "G0 yields a usable splat"
    );
    let outputs = engine.scheduler().store().outputs_for_job(&job_id).unwrap();
    assert!(outputs.iter().any(|o| o.kind == "splat"));
}

/// Integrated send-path gate: a drone publishes a capture's events over the
/// in-process [`LoopbackBearer`] via the same [`BearerLadder`] the drone-side
/// Atlas forwarder uses, and the compute node's receiver drain loop — the shape
/// the `ados-compute` daemon's `atlas_receiver_loop` runs (`while let Some(ev) =
/// rx.recv()`, terminating on channel close) — drains the bearer's channel,
/// ingests each event, and enqueues a reconstruct job on the terminal `Bagged`
/// state. Proves forwarder → bearer → receiver → [`AtlasIngest::ingest`] →
/// enqueue composes in-process with no TCP, GPU, camera, or RF.
#[tokio::test]
async fn integrated_loopback_capture_drains_into_an_enqueued_reconstruct_job() {
    let engine = engine();

    // ── Drone side: a capture's events ride the bearer ladder; the in-process
    //    loopback bearer is the local-first rung the ladder picks. ──
    let (bearer, mut rx) = LoopbackBearer::channel();
    let ladder = BearerLadder::new(vec![Box::new(bearer)]);

    const N: usize = 5;
    for i in 0..N {
        assert_eq!(
            ladder.send(&keyframe(i)).await.unwrap(),
            BearerKind::Loopback,
            "the keyframe rode the in-process loopback bearer"
        );
    }
    assert_eq!(
        ladder.send(&bagged(N as u64)).await.unwrap(),
        BearerKind::Loopback,
        "the bagged capture-state rode the loopback bearer"
    );

    // Drop the lane so the drain loop terminates on channel close, exactly as the
    // daemon's receiver loop does on shutdown.
    drop(ladder);

    // ── Compute side: the receiver drain loop. One `AtlasIngest` for the session
    //    counts keyframes and, on the `Bagged` state, submits the reconstruct job
    //    the workers pick up (mirrors `atlas_receiver_loop`, run inline against
    //    the engine's store). ──
    let store = engine.scheduler().store();
    let mut ingest = AtlasIngest::new();
    let mut enqueued_job = None;
    while let Some(event) = rx.recv().await {
        if let Some(job_id) = ingest.ingest(&event, store, 200).unwrap() {
            enqueued_job = Some(job_id);
        }
    }

    // The bagged session enqueued exactly the reconstruct job, queued for a worker.
    let job_id = enqueued_job.expect("the bagged capture enqueued a reconstruct job");
    assert_eq!(
        ingest.keyframes_seen(),
        N as u64,
        "every keyframe drained into the node"
    );
    let job = store
        .get_job(&job_id)
        .unwrap()
        .expect("the enqueued job is in the store");
    assert_eq!(job.kind, ComputeJobKind::Reconstruct);
    assert_eq!(
        job.state,
        ComputeJobState::Queued,
        "the reconstruct job is queued for a worker"
    );
    assert_eq!(
        store.count_in_state(ComputeJobState::Queued).unwrap(),
        1,
        "the bagged session enqueued exactly one reconstruct job"
    );
}

/// Perception-offload gate: an NPU-less drone offloads a frame; the node runs the
/// (mock) detector and returns a detection.
#[tokio::test]
async fn perception_offload_runs_the_detector_and_returns_a_detection() {
    let engine = engine();
    engine
        .scheduler()
        .store()
        .submit_job(&JobRecord {
            id: "off-1".into(),
            kind: ComputeJobKind::PerceptionOffload,
            dataset_id: None,
            state: ComputeJobState::Queued,
            progress: 0.0,
            params: serde_json::json!({
                "frame": { "camera_id": "front", "width": 640, "height": 640, "ts_ms": 100 }
            }),
            result_ref: None,
            error: None,
            created_ms: 100,
            updated_ms: 100,
        })
        .unwrap();

    let outcome = engine.tick(200).unwrap().expect("offload job claimed");
    assert_eq!(outcome.state, ComputeJobState::Completed);
    assert!(
        !outcome.detections.is_empty(),
        "the perception offload returns at least one detection"
    );
}

/// Cluster gate: a master reports its own role and aggregates a registered slave's
/// idle capacity (the master/slave compute cluster, single-master v1).
#[test]
fn cluster_master_aggregates_a_registered_slave() {
    let mut engine = engine();
    assert_eq!(engine.heartbeat().unwrap().role, ComputeRole::Master);

    let before = engine.heartbeat().unwrap().cluster.aggregate_workers_idle;
    engine.cluster_mut().register_slave(SlaveDescriptor {
        node_id: "gpu-b".into(),
        accelerators: vec!["cuda:0".into()],
        workers_idle: 4,
        queue_depth: 0,
    });
    let hb = engine.heartbeat().unwrap();
    assert_eq!(hb.cluster.slaves.len(), 1);
    assert_eq!(
        hb.cluster.aggregate_workers_idle,
        before + 4,
        "the slave's idle workers fold into the cluster capacity"
    );
}

/// A full keyframe envelope for one `camera_id`, with an IMU sample so the
/// camera subtree + the IMU scalars are both produced. Synthetic intrinsics +
/// an identity pose translated along x by the keyframe id (no real camera).
fn keyframe_env(camera_id: &str, kf_id: u64) -> KeyframeEnvelope {
    KeyframeEnvelope {
        session_id: "sitl".into(),
        kf_id,
        ts_unix_ms: 1000 + kf_id as i64,
        camera_id: camera_id.into(),
        camera_role: CameraRole::Primary,
        tier: KeyframeTier::Full,
        image: KeyframeImage {
            encoding: ImageEncoding::Jpeg,
            width: 1280,
            height: 720,
            bytes: vec![],
        },
        camera: CameraIntrinsics {
            k: [900.0, 0.0, 640.0, 0.0, 900.0, 360.0, 0.0, 0.0, 1.0],
            distortion: Distortion {
                model: "radtan".into(),
                params: vec![],
            },
        },
        pose: Pose {
            r: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            t: [kf_id as f64, 0.0, 0.0],
            cov: None,
        },
        pose_source: PoseSource::LocalVio,
        global_anchor: None,
        imu_window: vec![ImuSample {
            t_ms: 999,
            gyro: [0.1, 0.2, 0.3],
            accel: [0.0, 0.0, 9.81],
        }],
        flags: KeyframeFlags::default(),
    }
}

/// Mirror the delta WS handler's per-device filter against one subscriber: a
/// tuple tagged for another device is dropped from this device's view. The
/// broadcast delivers the same tuple to every subscriber, so this receives
/// exactly one and applies the `dev != device_id` filter, returning the event
/// only when it belongs to `device_id`.
async fn deliver_for(
    rx: &mut broadcast::Receiver<(String, AtlasEvent)>,
    device_id: &str,
) -> Option<AtlasEvent> {
    match rx.recv().await {
        Ok((dev, event)) if dev == device_id => Some(event),
        Ok(_) => None, // another drone's delta — not this view
        Err(_) => None,
    }
}

/// G1: the Rerun recording maps a multi-keyframe single-camera capture + the
/// reconstructed splat onto the entity tree the GCS viewer renders — the camera
/// subtree, the IMU scalars, a once-logged camera intrinsics across the run, and
/// the verbatim wire discriminators (`Transform3D`, `SplatSlab`).
#[tokio::test]
async fn g1_rerun_recording_maps_keyframes_for_the_gcs_viewer() {
    const N: usize = 4;
    let mut rec = RerunRecording::new();
    for i in 0..N {
        rec.push_keyframe(&keyframe_env("cam-front", i as u64));
    }
    rec.push_splat(
        &SplatDescriptor {
            gaussian_count: 4800,
            step: 200,
            url: Some("spz://sitl".into()),
            handle: None,
        },
        2000,
    );

    let paths: Vec<&str> = rec.entries.iter().map(|e| e.entity_path.as_str()).collect();
    assert!(
        paths.contains(&"world/camera/cam-front"),
        "the camera has a subtree the viewer renders"
    );
    assert!(
        paths.contains(&"world/camera/cam-front/rgb"),
        "the camera's image rides world/camera/<id>/rgb"
    );
    assert!(
        paths.contains(&"world/imu/accel/z"),
        "the IMU window is logged as per-axis scalars"
    );

    // The camera's static intrinsics is logged exactly once across the N
    // keyframes (the viewer needs the Pinhole once, not per frame)...
    let pinholes = rec
        .entries
        .iter()
        .filter(|e| matches!(e.archetype, RerunArchetype::Pinhole { .. }))
        .count();
    assert_eq!(
        pinholes, 1,
        "the camera intrinsics is logged once across the whole run"
    );
    // ...while every keyframe contributes its own pose transform.
    let transforms = rec
        .entries
        .iter()
        .filter(|e| matches!(e.archetype, RerunArchetype::Transform3D { .. }))
        .count();
    assert_eq!(transforms, N, "every keyframe contributes a pose transform");

    // The serialized manifest carries the verbatim Rerun archetype names the
    // GCS viewer maps (not snake_case digit-splits).
    let json = rec.to_json().unwrap();
    assert!(
        json.contains("\"Transform3D\""),
        "pose transform discriminator"
    );
    assert!(json.contains("\"SplatSlab\""), "splat slab discriminator");
    assert!(!json.contains("transform3_d"), "no mangled discriminator");
}

/// G2: a captured bag rides a bearer into the receiver/ingest, drains into one
/// reconstruct job, and — one step past the integrated-loopback gate, which
/// stops at "queued" — the worker runs it to a delivered splat output.
#[tokio::test]
async fn g2_bag_pipeline_reconstructs_to_a_delivered_output() {
    let engine = engine();

    // The PostFlightBulk bearer is not implemented; the in-process loopback
    // bearer stands in for the post-flight-bulk LAN lane here (identical
    // AtlasEvent contract, no GPU / camera / RF).
    let (bearer, mut rx) = LoopbackBearer::channel();
    let ladder = BearerLadder::new(vec![Box::new(bearer)]);

    const N: usize = 6;
    for i in 0..N {
        ladder.send(&keyframe(i)).await.unwrap();
    }
    ladder.send(&bagged(N as u64)).await.unwrap();
    drop(ladder);

    let store = engine.scheduler().store();
    let mut ingest = AtlasIngest::new();
    let mut job_id = None;
    while let Some(ev) = rx.recv().await {
        if let Some(id) = ingest.ingest(&ev, store, 200).unwrap() {
            job_id = Some(id);
        }
    }
    let job_id = job_id.expect("the bagged capture enqueued a reconstruct job");

    // The dataset is a bag carrying the camera count + the received-keyframe
    // proof (the drone's send is fire-and-forget, so only decoded frames count).
    let job = store.get_job(&job_id).unwrap().expect("the enqueued job");
    let dataset_id = job
        .dataset_id
        .clone()
        .expect("the reconstruct job references a dataset");
    let dataset = store
        .get_dataset(&dataset_id)
        .unwrap()
        .expect("the bag dataset was inserted");
    assert_eq!(dataset.kind, "bag");
    assert_eq!(dataset.meta["cameras"], 1);
    assert_eq!(dataset.meta["received_keyframes"], N as u64);

    // Exactly one fused reconstruct job, queued for a worker.
    assert_eq!(
        store.count_in_state(ComputeJobState::Queued).unwrap(),
        1,
        "the bag enqueued exactly one reconstruct job"
    );

    // The worker runs it to a delivered splat output (past "queued").
    let outcome = engine.tick(300).unwrap().expect("the queued job ran");
    assert_eq!(outcome.job_id, job_id);
    assert_eq!(outcome.state, ComputeJobState::Completed);
    let outputs = store.outputs_for_job(&job_id).unwrap();
    assert!(
        outputs.iter().any(|o| o.kind == "splat"),
        "the bag reconstructs to a delivered splat output"
    );
}

/// G3: the shared-data delta lane isolates per device — one drone's world model
/// never crosses into another drone's plugin view — and an NPU-less drone's
/// perception offload runs the (mock) detector and returns a detection.
#[tokio::test]
async fn g3_plugin_data_share_isolates_per_device_and_offloads() {
    let broadcaster = DeltaBroadcaster::new(16);
    let mut rx_one = broadcaster.subscribe();
    let mut rx_two = broadcaster.subscribe();
    assert_eq!(broadcaster.subscriber_count(), 2);

    // One drone's splat update is published for its device only.
    let splat = AtlasEvent {
        topic: PLUGIN_ATLAS_SPLAT_TOPIC.into(),
        payload: vec![1, 2, 3],
    };
    broadcaster.publish("drone-1", splat);

    // drone-1's plugin view receives it; drone-2's view filters it out.
    let for_one = deliver_for(&mut rx_one, "drone-1").await;
    let for_two = deliver_for(&mut rx_two, "drone-2").await;
    assert_eq!(
        for_one.as_ref().map(|e| e.topic.as_str()),
        Some(PLUGIN_ATLAS_SPLAT_TOPIC),
        "drone-1's plugin view receives its own world delta"
    );
    assert!(
        for_two.is_none(),
        "drone-2's plugin view never sees drone-1's world delta (device isolation)"
    );

    // The NPU-less offload path: the node runs the detector and returns a result.
    let engine = engine();
    engine
        .scheduler()
        .store()
        .submit_job(&JobRecord {
            id: "off-iso".into(),
            kind: ComputeJobKind::PerceptionOffload,
            dataset_id: None,
            state: ComputeJobState::Queued,
            progress: 0.0,
            params: serde_json::json!({
                "frame": { "camera_id": "front", "width": 640, "height": 640, "ts_ms": 100 }
            }),
            result_ref: None,
            error: None,
            created_ms: 100,
            updated_ms: 100,
        })
        .unwrap();
    let outcome = engine.tick(200).unwrap().expect("offload job claimed");
    assert_eq!(outcome.state, ComputeJobState::Completed);
    assert!(
        !outcome.detections.is_empty(),
        "the perception offload returns at least one detection"
    );
}

/// G4: a live session walks pairing -> ready -> active, trains on each ingested
/// keyframe (monotonic gaussian/step growth, SPZ-framed deltas), pauses to stop
/// ingest, and every emitted delta fans out to the device's Live World
/// subscriber over the delta broadcaster.
#[tokio::test]
async fn g4_live_stream_delta_fanout() {
    let mut session = LiveSession::new("live-1", 0);
    assert!(session.try_transition(LiveSessionState::Ready, 1));
    assert!(session.try_transition(LiveSessionState::Active, 2));

    let broadcaster = DeltaBroadcaster::new(64);
    let mut rx = broadcaster.subscribe();

    const N: u64 = 4;
    let mut last_gaussians = 0u64;
    let mut last_step = 0u64;
    for i in 0..N {
        let (desc, delta) = session
            .ingest_keyframe(&MockDeltaProducer, None, 10 + i as i64)
            .expect("an active session trains on each keyframe");
        assert!(
            desc.gaussian_count > last_gaussians,
            "the trainer's gaussian count grows per keyframe"
        );
        assert!(
            desc.step > last_step,
            "the training step advances per keyframe"
        );
        assert_eq!(desc.gaussian_count, delta.gaussian_count);
        assert_eq!(desc.step, delta.step);
        last_gaussians = desc.gaussian_count;
        last_step = desc.step;
        // The delta frame carries the SPZ magic the Live World decoder reads.
        assert_eq!(
            &delta.bytes[0..4],
            b"SPZ0",
            "the delta carries the SPZ frame magic"
        );
        // Fan the delta out to the device's Live World subscriber.
        broadcaster.publish(
            "drone-live",
            AtlasEvent {
                topic: PLUGIN_ATLAS_SPLAT_TOPIC.into(),
                payload: delta.bytes.clone(),
            },
        );
    }

    // A paused session drops ingest (produces no further delta).
    assert!(session.try_transition(LiveSessionState::Paused, 100));
    assert!(
        session
            .ingest_keyframe(&MockDeltaProducer, None, 101)
            .is_none(),
        "a paused session drops ingest"
    );

    // The broadcaster delivered exactly N deltas to the device's subscriber.
    let mut delivered = 0u64;
    for _ in 0..N {
        let (dev, ev) = rx.recv().await.expect("each published delta is on the bus");
        assert_eq!(dev, "drone-live");
        assert_eq!(ev.topic, PLUGIN_ATLAS_SPLAT_TOPIC);
        delivered += 1;
    }
    assert_eq!(
        delivered, N,
        "every live delta reached the device's subscriber"
    );
    assert!(
        rx.try_recv().is_err(),
        "the paused ingest published nothing further"
    );
}

/// G5: a multi-camera capture fuses into one world — N distinct cameras' frames
/// drain into a single bag dataset whose camera count is N and a single
/// reconstruct job (not one per camera), and the same frames map onto N distinct
/// camera subtrees in the recording, each with its intrinsics logged exactly
/// once (per-camera dedup at scale).
#[tokio::test]
async fn g5_multi_cam_fuses_into_one_world() {
    let engine = engine();
    const N: usize = 4;
    const FRAMES_PER_CAM: usize = 2;

    // The same keyframes feed both the ingest path and the viewer recording:
    // each camera appears in FRAMES_PER_CAM frames, so the per-camera dedup has
    // a repeat to drop.
    let mut keyframes = Vec::new();
    for round in 0..FRAMES_PER_CAM {
        for i in 0..N {
            keyframes.push(keyframe_env(&format!("cam-{i}"), (round * N + i) as u64));
        }
    }

    let mut rec = RerunRecording::new();
    let (bearer, mut rx) = LoopbackBearer::channel();
    let ladder = BearerLadder::new(vec![Box::new(bearer)]);
    for kf in &keyframes {
        ladder
            .send(&AtlasEvent {
                topic: ATLAS_KEYFRAME_TOPIC.into(),
                payload: kf.to_msgpack().unwrap(),
            })
            .await
            .unwrap();
        rec.push_keyframe(kf);
    }
    // The terminal bagged state declares N enabled cameras (the fusion key).
    let status = CaptureStatus {
        session_id: "multicam".into(),
        state: CaptureState::Bagged,
        keyframes: keyframes.len() as u64,
        vio_health: VioHealth::Good,
        camera_count: N as u32,
        ingest_rate_hz: 9.0,
    };
    ladder
        .send(&AtlasEvent {
            topic: ATLAS_CAPTURE_STATE_TOPIC.into(),
            payload: status.to_msgpack().unwrap(),
        })
        .await
        .unwrap();
    drop(ladder);

    let store = engine.scheduler().store();
    let mut ingest = AtlasIngest::new();
    let mut job_id = None;
    while let Some(ev) = rx.recv().await {
        if let Some(id) = ingest.ingest(&ev, store, 200).unwrap() {
            job_id = Some(id);
        }
    }
    let job_id = job_id.expect("the multi-cam bag enqueued a reconstruct job");

    // One fused dataset carrying all N cameras + every drained frame.
    let job = store.get_job(&job_id).unwrap().expect("the enqueued job");
    let dataset_id = job
        .dataset_id
        .clone()
        .expect("the reconstruct job references a dataset");
    let dataset = store
        .get_dataset(&dataset_id)
        .unwrap()
        .expect("the multi-cam bag dataset");
    assert_eq!(
        dataset.meta["cameras"], N as u64,
        "the fused dataset declares all N cameras"
    );
    assert_eq!(
        dataset.meta["received_keyframes"],
        (N * FRAMES_PER_CAM) as u64,
        "every camera's frames drained into the one bag"
    );
    // A single fused reconstruct job, not one per camera.
    assert_eq!(
        store.count_in_state(ComputeJobState::Queued).unwrap(),
        1,
        "the multi-cam capture fuses into one reconstruct job"
    );

    // The recording carries N distinct camera subtrees, each Pinhole logged once.
    for i in 0..N {
        let cam = format!("world/camera/cam-{i}");
        assert!(
            rec.entries.iter().any(|e| e.entity_path == cam),
            "camera {i} has its own subtree"
        );
    }
    let mut pinhole_paths: Vec<&str> = rec
        .entries
        .iter()
        .filter(|e| matches!(e.archetype, RerunArchetype::Pinhole { .. }))
        .map(|e| e.entity_path.as_str())
        .collect();
    assert_eq!(
        pinhole_paths.len(),
        N,
        "one intrinsics per distinct camera (per-camera dedup at scale)"
    );
    pinhole_paths.sort();
    pinhole_paths.dedup();
    assert_eq!(
        pinhole_paths.len(),
        N,
        "the N intrinsics sit on N distinct camera paths"
    );
}
