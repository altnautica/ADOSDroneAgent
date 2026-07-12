//! Real-model proof for the perception-offload detector on the workstation.
//! `#[ignore]` by default: it needs the COCO YOLOv8n ONNX present and the `onnx`
//! (or `coreml`) cargo feature, so a CI build with no model still passes. Run it
//! on the Mac to prove the real offload path end-to-end (the W1 gate):
//!
//!   cargo test -p ados-compute --features coreml --test onnx_offload -- --ignored
//!
//! It builds the `OnnxDetector` the compute node hosts, runs a real ONNX session
//! (on the Apple GPU under `coreml`) over a frame, and asserts the offload
//! returns a well-formed, normalized detection set — the same shape a local NPU
//! detection would land on the drone's bus.

#![cfg(feature = "onnx")]

use std::path::PathBuf;

use ados_compute::{Detector, FrameRef, OnnxDetector};
use ados_protocol::framebus::{
    DetectionHead, FrameFormat, ModelExecution, ModelKind, ModelMetadata,
};

/// The sideloaded COCO YOLOv8n ONNX. Override with `ADOS_TEST_COCO_ONNX`.
fn model_path() -> PathBuf {
    if let Ok(p) = std::env::var("ADOS_TEST_COCO_ONNX") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("ws1-model-run/yolov8n_coco_640.onnx")
}

fn coco_meta(path: &std::path::Path) -> ModelMetadata {
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

#[test]
#[ignore = "needs the COCO ONNX model sideloaded; run with --ignored"]
fn onnx_offload_detector_runs_a_real_session() {
    let path = model_path();
    assert!(
        path.exists(),
        "model not found at {} (set ADOS_TEST_COCO_ONNX)",
        path.display()
    );

    let detector = OnnxDetector::from_model(&coco_meta(&path)).expect("load coco onnx detector");
    assert!(detector.is_inference_capable());
    assert!(detector.name().starts_with("onnx:"));

    // A 640x640 mid-grey rgb24 frame: it need not contain a person, but the real
    // session + decode must run and return a well-formed (normalized, in-range)
    // detection set. A missing-pixels call must error, never fabricate a box.
    let frame = FrameRef {
        camera_id: "front".into(),
        width: 640,
        height: 640,
        ts_ms: 1,
    };
    assert!(
        detector.infer(&frame, None).is_err(),
        "a real detector must reject a pixel-less offload frame"
    );

    let pixels = vec![128u8; 640 * 640 * 3];
    let dets = detector
        .infer(&frame, Some(&pixels))
        .expect("real onnx inference runs");
    for d in &dets {
        assert!(d.confidence >= 0.0 && d.confidence <= 1.0);
        for v in d.bbox {
            assert!((-0.01..=1.01).contains(&v), "bbox out of range: {v}");
        }
    }
    eprintln!(
        "onnx offload ran on backend `{}`: {} detection(s)",
        detector.name(),
        dets.len()
    );
}
