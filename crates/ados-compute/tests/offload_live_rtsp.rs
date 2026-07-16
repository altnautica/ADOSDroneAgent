//! Live-rig proof for the workstation offload path: pull a real drone's RTSP
//! camera through the streaming offload session and run the real ONNX detector
//! (CoreML on macOS) over it — the exact production path (`RtspFrameStream` →
//! `run_offload_session` → real `OnnxDetector`) minus the job daemon.
//!
//! `#[ignore]` by default: it needs a live RTSP camera + the COCO model. Run it
//! on the workstation with a drone streaming (e.g. skynodepi at
//! `rtsp://192.168.200.179:8554/main`):
//!
//!   ADOS_TEST_RTSP=rtsp://192.168.200.179:8554/main \
//!     cargo test -p ados-compute --features coreml --test offload_live_rtsp -- --ignored --nocapture

#![cfg(feature = "onnx")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ados_compute::{run_offload_session, Detector, OnnxDetector, RtspFrameStream, SessionProgress};
use ados_protocol::framebus::{
    DetectionHead, FrameFormat, ModelExecution, ModelKind, ModelMetadata,
};
use tokio::sync::Notify;

fn model_path() -> PathBuf {
    if let Ok(p) = std::env::var("ADOS_TEST_COCO_ONNX") {
        return PathBuf::from(p);
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join("ws1-model-run/yolov8n_coco_640.onnx")
}

fn rtsp_url() -> String {
    std::env::var("ADOS_TEST_RTSP").unwrap_or_else(|_| "rtsp://192.168.200.179:8554/main".into())
}

fn coco_meta(path: &Path) -> ModelMetadata {
    ModelMetadata {
        id: "coco-yolov8n".into(),
        kind: ModelKind::Detection,
        execution: ModelExecution::EngineRun,
        input_width: 640,
        input_height: 640,
        input_format: FrameFormat::Rgb24,
        output_classes: Vec::new(),
        model_path: Some(path.to_string_lossy().into_owned()),
        head: DetectionHead::Yolo8,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live RTSP camera + the COCO model; run with --ignored"]
async fn offload_pulls_a_live_rtsp_camera_and_detects() {
    let path = model_path();
    assert!(
        path.exists(),
        "model not found at {} (set ADOS_TEST_COCO_ONNX)",
        path.display()
    );
    let detector: Arc<dyn Detector> =
        Arc::new(OnnxDetector::from_model(&coco_meta(&path)).expect("load coco onnx detector"));

    let url = rtsp_url();
    eprintln!("offload: pulling {url} through the streaming session...");
    let stream = RtspFrameStream::new("front", url.clone(), 1280, 720);

    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let cancel = Arc::new(Notify::new());
    let cancel2 = cancel.clone();
    let session = tokio::spawn(async move {
        run_offload_session(
            "live-1",
            "front",
            stream,
            detector,
            tx,
            cancel2,
            SessionProgress::detached(),
        )
        .await;
    });

    let mut batches = 0usize;
    let mut total_dets = 0usize;
    let deadline = tokio::time::sleep(Duration::from_secs(25));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            b = rx.recv() => match b {
                Some(batch) => {
                    batches += 1;
                    total_dets += batch.detections.len();
                    let labels: Vec<(String, u32)> = batch
                        .detections
                        .iter()
                        .map(|d| (d.class.clone(), (d.confidence * 100.0) as u32))
                        .collect();
                    eprintln!(
                        "  batch {batches} (seq {}): {} detection(s) {labels:?}",
                        batch.seq,
                        batch.detections.len()
                    );
                    if batches >= 5 {
                        break;
                    }
                }
                None => break,
            },
            _ = &mut deadline => {
                eprintln!("  (deadline reached)");
                break;
            }
        }
    }
    cancel.notify_waiters();
    let _ = session.await;

    eprintln!(
        "LIVE OFFLOAD PROVEN: {batches} batch(es), {total_dets} total detection(s) from {url}"
    );
    assert!(
        batches >= 1,
        "expected at least one frame to flow from the live RTSP camera into the offload session"
    );
}
