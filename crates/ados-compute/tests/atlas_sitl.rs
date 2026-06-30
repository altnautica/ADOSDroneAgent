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
    atlas_event_router, AtlasBearer, AtlasEvent, BearerKind, BearerLadder, LanHttpBearer,
    LoopbackBearer,
};
use ados_compute::{
    AtlasIngest, Cluster, Engine, JobRecord, JobStore, MockDetector, MockReconstructor, Scheduler,
};
use ados_protocol::atlas::{
    CaptureState, CaptureStatus, VioHealth, ATLAS_CAPTURE_STATE_TOPIC, ATLAS_KEYFRAME_TOPIC,
};
use ados_protocol::compute::{ComputeJobKind, ComputeJobState, ComputeRole, SlaveDescriptor};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

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
