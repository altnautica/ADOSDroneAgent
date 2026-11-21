//! Real-model smoke test for the ONNX depth path. `#[ignore]` by default: it
//! needs the Depth Anything V2 Small ONNX
//! (`depth_anything_v2_small_252x196.onnx`, built by
//! `scripts/models/export_depth_anything_v2.py`) and the `onnx` cargo feature.
//! Run it once the model is on disk:
//!
//!   ADOS_TEST_DEPTH_ONNX=/path/depth_anything_v2_small_252x196.onnx \
//!     ORT_DYLIB_PATH=/path/libonnxruntime.so \
//!     cargo test -p ados-vision-engine --features onnx --test onnx_depth_infer -- --ignored --nocapture
//!
//! It proves the model loads into a two-thread ORT session, returns a
//! full-frame relative inverse depth map, ranks a near band below a far one,
//! and prints the per-run latency the CPU budget is judged on.

#![cfg(feature = "onnx")]

use std::path::PathBuf;
use std::time::Instant;

use ados_protocol::framebus::{
    DetectionHead, FrameFormat, ModelExecution, ModelKind, ModelMetadata,
};
use ados_vision_engine::backend::{OnnxBackend, VisionBackend};

fn model_path() -> PathBuf {
    PathBuf::from(
        std::env::var("ADOS_TEST_DEPTH_ONNX")
            .expect("set ADOS_TEST_DEPTH_ONNX to the depth ONNX model path"),
    )
}

/// A 640x360 frame: a smooth bright sky over a coarse ground texture whose
/// grain grows toward the bottom, the cue a depth model reads as "nearer".
fn scene() -> Vec<u8> {
    let (w, h) = (640usize, 360usize);
    let mut px = vec![0u8; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 3;
            if y < h / 2 {
                px[i] = 150;
                px[i + 1] = 190;
                px[i + 2] = 235;
            } else {
                let grain = 2 + (y - h / 2) / 8;
                let v = if ((x / grain) + (y / grain)) % 2 == 0 {
                    90
                } else {
                    140
                };
                px[i] = v;
                px[i + 1] = (v as f32 * 0.8) as u8;
                px[i + 2] = (v as f32 * 0.5) as u8;
            }
        }
    }
    px
}

#[test]
#[ignore = "needs the Depth Anything V2 Small ONNX on disk; run with --ignored"]
fn depth_anything_small_returns_a_full_frame_map_and_ranks_ground_nearer_than_sky() {
    let path = model_path();
    assert!(path.is_file(), "model not found at {}", path.display());
    let meta = ModelMetadata {
        id: "depth_anything_v2_small".into(),
        kind: ModelKind::Depth,
        execution: ModelExecution::EngineRun,
        input_width: 252,
        input_height: 196,
        input_format: FrameFormat::Rgb24,
        output_classes: Vec::new(),
        model_path: Some(path.to_string_lossy().into_owned()),
        head: DetectionHead::Yolo8,
    };
    let model = OnnxBackend::new(1, 2).load(&meta).expect("load depth onnx");
    let frame = scene();
    let mut latencies = Vec::new();
    let mut last = None;
    for _ in 0..11 {
        let t0 = Instant::now();
        let map = model
            .infer_dense(&frame, 640, 360, FrameFormat::Rgb24)
            .expect("depth inference runs");
        latencies.push(t0.elapsed().as_secs_f64() * 1000.0);
        last = Some(map);
    }
    let map = last.unwrap();
    assert_eq!((map.width, map.height), (252, 196));
    assert!(map.data.iter().all(|v| v.is_finite()));
    let sky = map.at(0.5, 0.15);
    let ground = map.at(0.5, 0.9);
    assert!(ground > sky, "near ground {ground} should exceed sky {sky}");
    latencies.remove(0);
    latencies.sort_by(f64::total_cmp);
    println!(
        "depth anything v2 small 252x196, 2 threads: median {:.1} ms, max {:.1} ms over {} runs",
        latencies[latencies.len() / 2],
        latencies[latencies.len() - 1],
        latencies.len()
    );
}
