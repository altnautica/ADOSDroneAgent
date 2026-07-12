//! SITL offload gate (no hardware): the whole perception-offload return path,
//! wired in-process, with a mock detector.
//!
//! frames → node streaming session → detection broadcaster → WS → drone
//! subscriber → return bridge (ados-offload safety gate) → real
//! `vision.sock` server (a `VisionEngine`) → the `vision.detection` bus.
//!
//! This closes the R6 offload loop in CI: synthetic frames go in on the node
//! side and boxes come out on the drone's actual detection bus, with the freshness
//! + lock safety gate exercised. No GPU, no camera, no RTSP, no RF.

use std::sync::Arc;
use std::time::Duration;

use ados_compute::{
    offload_ws_path, offload_ws_router, pump_to_broadcaster, run_offload_session,
    stream_offload_detections, DetectionBroadcaster, MockDetector, OffloadReturnBridge,
    VecFrameStream, VisionSockPublisher,
};
use ados_offload::OffloadMode;
use ados_protocol::framebus::DetectionBatch;
use ados_protocol::offload::OffloadDetectionBatch;
use ados_vision::backend::MockBackend;
use ados_vision::engine::VisionEngine;
use tokio::sync::Notify;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Spin the node's detection WS return-stream server on a loopback port.
async fn spawn_ws_server(broadcaster: Arc<DetectionBroadcaster>) -> std::net::SocketAddr {
    let app = offload_ws_router(broadcaster);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// Spin a real `vision.sock` server backed by a `VisionEngine`, and return the
/// engine (so the test subscribes to its detection bus) + the socket path.
async fn spawn_vision_sock(dir: &std::path::Path) -> (Arc<VisionEngine>, String, Arc<Notify>) {
    let engine = VisionEngine::new(Box::new(MockBackend), 4);
    let sock = dir.join("vision.sock").to_string_lossy().to_string();
    let cancel = Arc::new(Notify::new());
    let se = engine.clone();
    let ss = sock.clone();
    let sc = cancel.clone();
    tokio::spawn(async move {
        ados_vision::visionsock::serve(se, &ss, sc).await.unwrap();
    });
    // Give the socket a moment to bind before a client connects.
    tokio::time::sleep(Duration::from_millis(50)).await;
    (engine, sock, cancel)
}

#[tokio::test]
async fn offload_frames_reach_the_drone_vision_bus_end_to_end() {
    // --- node side: session -> broadcaster -> WS ------------------------------
    let broadcaster = Arc::new(DetectionBroadcaster::new(64));
    let ws_addr = spawn_ws_server(broadcaster.clone()).await;

    // The streaming session pumps its detection channel into the broadcaster.
    let (sess_tx, sess_rx) = tokio::sync::mpsc::channel::<OffloadDetectionBatch>(64);
    let bpump = broadcaster.clone();
    tokio::spawn(pump_to_broadcaster(sess_rx, bpump));

    // --- drone side: subscribe over WS ---------------------------------------
    let dir = tempfile::tempdir().unwrap();
    let (engine, sock_path, vision_cancel) = spawn_vision_sock(dir.path()).await;
    let mut bus = engine.subscribe_detections();

    let (det_tx, mut det_rx) = tokio::sync::mpsc::channel::<OffloadDetectionBatch>(64);
    let ws_url = format!("ws://{ws_addr}{}", offload_ws_path("sess-1"));
    let sub_cancel = Arc::new(Notify::new());
    let sc = sub_cancel.clone();
    let subscriber = tokio::spawn(async move {
        stream_offload_detections(&ws_url, det_tx, sc)
            .await
            .unwrap();
    });

    // The return bridge: gate + convert + publish onto the drone vision bus.
    let mut bridge = OffloadReturnBridge::new(OffloadMode::VisionOnly, 1_000, 1_000, "offload");
    let mut publisher = VisionSockPublisher::new(sock_path);
    let bridge_task = tokio::spawn(async move {
        let mut published: Vec<DetectionBatch> = Vec::new();
        while let Some(batch) = det_rx.recv().await {
            let (db, _status) = bridge.ingest(&batch, now_ms());
            // Publish onto the drone's real vision.detection bus.
            publisher.publish(&db).await.unwrap();
            published.push(db);
            if published.len() == 3 {
                break;
            }
        }
        published
    });

    // Wait for the WS subscriber to register before the node starts emitting, so
    // no batch is dropped in the connect window.
    for _ in 0..200 {
        if broadcaster.subscriber_count() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // --- run the node session: 3 synthetic frames ----------------------------
    let stream = VecFrameStream::solid("front", 64, 48, 3, 5000);
    let detector: Arc<dyn ados_compute::Detector> = Arc::new(MockDetector);
    let session_cancel = Arc::new(Notify::new());
    let sc2 = session_cancel.clone();
    let session = tokio::spawn(async move {
        run_offload_session("sess-1", "front", stream, detector, sess_tx, sc2).await;
    });

    // --- assert: 3 batches reached the actual vision detection bus ------------
    let mut received = Vec::new();
    for _ in 0..3 {
        let batch = tokio::time::timeout(Duration::from_secs(10), bus.recv())
            .await
            .expect("a detection batch on the drone bus within 10s")
            .expect("bus not closed");
        received.push(batch);
    }

    assert_eq!(received.len(), 3, "3 offloaded frames -> 3 bus batches");
    for (i, b) in received.iter().enumerate() {
        assert_eq!(b.model_id, "offload");
        assert_eq!(b.camera_id, "front");
        assert_eq!(
            b.frame_id, i as u64,
            "the session seq is the frame id on the bus"
        );
        assert_eq!(b.detections.len(), 1, "the mock returns one box per frame");
        // The normalized [0.4,0.4,0.2,0.2] mock box denormalized to 64x48.
        let bb = &b.detections[0].bbox;
        assert!(
            (bb.x - 0.4 * 64.0).abs() < 1e-3,
            "x denormalized to frame width"
        );
        assert!(
            (bb.y - 0.4 * 48.0).abs() < 1e-3,
            "y denormalized to frame height"
        );
    }

    let published = bridge_task.await.unwrap();
    assert_eq!(published.len(), 3);

    // Clean shutdown.
    session.await.unwrap();
    sub_cancel.notify_waiters();
    let _ = subscriber.await;
    vision_cancel.notify_waiters();
    let _ = session_cancel; // (kept for symmetry; the vec stream ends on its own)
}
