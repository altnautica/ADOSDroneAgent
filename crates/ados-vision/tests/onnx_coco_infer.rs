//! Real-model smoke test for the in-process ONNX detector path (the SITL /
//! CPU build). `#[ignore]` by default: it needs the COCO YOLOv8n ONNX present
//! and the `onnx` cargo feature, so CI without the model still passes. Run it
//! explicitly once the model is sideloaded:
//!
//!   cargo test -p ados-vision --features onnx --test onnx_coco_infer -- --ignored
//!
//! It proves the real model loads into an ORT session and one inference call
//! returns a valid (possibly empty) detection set without panicking — the
//! end-to-end CPU detector path. A real person-detection assertion belongs to
//! the SITL run with real video; here a synthetic frame just exercises the path.

#![cfg(feature = "onnx")]

use std::path::PathBuf;

use ados_protocol::framebus::{
    DetectionHead, FrameFormat, ModelExecution, ModelKind, ModelMetadata,
};
use ados_vision::backend::{OnnxBackend, VisionBackend};

/// The sideloaded COCO YOLOv8n ONNX. Override with `ADOS_TEST_COCO_ONNX`.
fn model_path() -> PathBuf {
    if let Ok(p) = std::env::var("ADOS_TEST_COCO_ONNX") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("ws1-model-run/yolov8n_coco_640.onnx")
}

#[test]
#[ignore = "needs the COCO ONNX model sideloaded; run with --ignored"]
fn coco_onnx_loads_and_infers_one_frame() {
    let path = model_path();
    assert!(
        path.is_file(),
        "model not found at {} (set ADOS_TEST_COCO_ONNX)",
        path.display()
    );

    let meta = ModelMetadata {
        id: "coco".into(),
        kind: ModelKind::Detection,
        execution: ModelExecution::EngineRun,
        input_width: 640,
        input_height: 640,
        input_format: FrameFormat::Rgb24,
        output_classes: coco80(),
        model_path: Some(path.to_string_lossy().into_owned()),
        head: DetectionHead::Yolo8,
    };

    let backend = OnnxBackend::new();
    let model = backend.load(&meta).expect("load coco onnx");

    // A mid-grey 640x640 rgb24 frame: no real person, but the path must run.
    let frame = vec![128u8; 640 * 640 * 3];
    let dets = model
        .infer(&frame, 640, 640, FrameFormat::Rgb24)
        .expect("inference runs");
    // The detector ran end-to-end; every detection it does return is well-formed.
    for d in &dets {
        assert!(d.confidence >= 0.0 && d.confidence <= 1.0);
        assert!(d.bbox.width >= 0.0 && d.bbox.height >= 0.0);
    }
}

fn coco80() -> Vec<String> {
    // The standard 80-class COCO label order (person = index 0).
    "person bicycle car motorcycle airplane bus train truck boat traffic_light \
     fire_hydrant stop_sign parking_meter bench bird cat dog horse sheep cow \
     elephant bear zebra giraffe backpack umbrella handbag tie suitcase frisbee \
     skis snowboard sports_ball kite baseball_bat baseball_glove skateboard \
     surfboard tennis_racket bottle wine_glass cup fork knife spoon bowl banana \
     apple sandwich orange broccoli carrot hot_dog pizza donut cake chair couch \
     potted_plant bed dining_table toilet tv laptop mouse remote keyboard \
     cell_phone microwave oven toaster sink refrigerator book clock vase \
     scissors teddy_bear hair_drier toothbrush"
        .split_whitespace()
        .map(|s| s.to_string())
        .collect()
}
