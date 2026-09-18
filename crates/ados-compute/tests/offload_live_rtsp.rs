//! Live-rig proof for the workstation offload path: pull a real drone's RTSP
//! camera through the streaming offload session and run the real ONNX detector
//! (CoreML on macOS) over it — the exact production path (`RtspFrameStream` →
//! `run_offload_session` → real `OnnxDetector`) minus the job daemon.
//!
//! `#[ignore]` by default: it needs a live RTSP camera and the COCO model.
//! Both are named by environment variable and there is NO fallback for
//! either — see `rtsp_url`. Run it on the workstation with a drone
//! streaming:
//!
//!   ADOS_TEST_RTSP=rtsp://<camera-host>:8554/main \
//!     ADOS_TEST_COCO_ONNX=/path/to/yolov8n_coco_640.onnx \
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

/// The COCO model, or `None` when the run is not configured for it.
fn model_path() -> Option<PathBuf> {
    std::env::var("ADOS_TEST_COCO_ONNX").ok().map(PathBuf::from)
}

/// The camera to pull from, or `None` when the run is not configured for it.
///
/// No default. This used to fall back to a hardcoded address on someone's
/// bench, which is two defects in one line: the moment the surrounding CI
/// job was switched on the test went red against a host that does not
/// exist there, and a real network address was published in a public
/// repository, where git history keeps it forever.
fn rtsp_url() -> Option<String> {
    std::env::var("ADOS_TEST_RTSP")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Both inputs, or a stated reason to skip.
///
/// A test with no rig should SKIP loudly, not pass quietly and not fail: a
/// green result from a run that connected to nothing is the same
/// manufactured confidence this audit found everywhere else.
fn live_inputs() -> Option<(String, PathBuf)> {
    match (rtsp_url(), model_path()) {
        (Some(url), Some(model)) if model.exists() => Some((url, model)),
        (None, _) => {
            eprintln!("SKIP: ADOS_TEST_RTSP is unset — no camera to pull from");
            None
        }
        (_, None) => {
            eprintln!("SKIP: ADOS_TEST_COCO_ONNX is unset — no detector model");
            None
        }
        (_, Some(model)) => {
            eprintln!(
                "SKIP: ADOS_TEST_COCO_ONNX does not exist: {}",
                model.display()
            );
            None
        }
    }
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
    let Some((url, path)) = live_inputs() else {
        return;
    };
    let detector: Arc<dyn Detector> =
        Arc::new(OnnxDetector::from_model(&coco_meta(&path)).expect("load coco onnx detector"));

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
