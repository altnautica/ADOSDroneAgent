//! Real-model smoke test for the in-process ONNX detector path (the SITL /
//! CPU build). `#[ignore]` by default: it needs the COCO YOLOv8n ONNX present
//! and the `onnx` cargo feature, so CI without the model still passes. Run it
//! explicitly once the model is sideloaded:
//!
//!   cargo test -p ados-vision --features onnx --test onnx_coco_infer -- --ignored
//!
//! It proves the real model loads into an ORT session and inference runs
//! end-to-end on the CPU path: one test runs on a synthetic frame (the path
//! always executes), and a second runs on a real decoded person frame
//! (`ADOS_TEST_PERSON_RGB`) and asserts the detector finds a real person — the
//! live perception check the SITL run leans on.

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
        let b = d.bbox.as_ref().expect("a box detector emits a bbox");
        assert!(b.width >= 0.0 && b.height >= 0.0);
    }
}

/// The real detector finds a real person. `#[ignore]` by default: it needs the
/// COCO ONNX plus a 640x640 rgb24 frame of a person at `ADOS_TEST_PERSON_RGB`
/// (decode any photo with ffmpeg: `-vf "scale=640:640:force_original_aspect_ratio
/// =decrease,pad=640:640:(ow-iw)/2:(oh-ih)/2,format=rgb24" -f rawvideo`). This is
/// the live perception check the SITL run leans on — real model, real person.
#[test]
#[ignore = "needs the COCO ONNX + a 640x640 rgb24 person frame; run with --ignored"]
fn coco_onnx_detects_a_real_person() {
    let model = model_path();
    assert!(model.is_file(), "model not found at {}", model.display());
    let Ok(frame_path) = std::env::var("ADOS_TEST_PERSON_RGB") else {
        panic!("set ADOS_TEST_PERSON_RGB to a 640x640 rgb24 person frame");
    };
    let frame = std::fs::read(&frame_path).expect("read person frame");
    assert_eq!(frame.len(), 640 * 640 * 3, "frame must be 640x640 rgb24");

    let meta = ModelMetadata {
        id: "coco".into(),
        kind: ModelKind::Detection,
        execution: ModelExecution::EngineRun,
        input_width: 640,
        input_height: 640,
        input_format: FrameFormat::Rgb24,
        output_classes: coco80(),
        model_path: Some(model.to_string_lossy().into_owned()),
        head: DetectionHead::Yolo8,
    };
    let model = OnnxBackend::new().load(&meta).expect("load coco onnx");
    let dets = model
        .infer(&frame, 640, 640, FrameFormat::Rgb24)
        .expect("inference runs");
    let people: Vec<_> = dets.iter().filter(|d| d.class_label == "person").collect();
    eprintln!(
        "real-frame inference: {} detections, {} people",
        dets.len(),
        people.len()
    );
    for p in &people {
        let b = p.bbox.as_ref().expect("a box detector emits a bbox");
        eprintln!(
            "  person conf={:.2} bbox=({:.0},{:.0},{:.0},{:.0})",
            p.confidence, b.x, b.y, b.width, b.height
        );
    }
    assert!(
        !people.is_empty(),
        "the COCO detector finds at least one real person"
    );
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
