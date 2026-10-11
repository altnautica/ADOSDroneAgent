//! Mock backends: the no-op default and the synthetic moving target.

use super::*;

/// Always-available no-op backend. Loads any model and returns no detections,
/// which keeps the engine and its plugins running while a real backend is
/// unavailable.
#[derive(Debug, Default, Clone)]
pub struct MockBackend;

struct MockModel;

impl LoadedModel for MockModel {
    fn infer(&self, _frame: &[u8], _w: u32, _h: u32, _f: FrameFormat) -> Result<Vec<Detection>> {
        Ok(Vec::new())
    }
}

impl VisionBackend for MockBackend {
    fn load(&self, _meta: &ModelMetadata) -> Result<Box<dyn LoadedModel>> {
        Ok(Box::new(MockModel))
    }
    fn name(&self) -> &str {
        "mock"
    }
    fn is_inference_capable(&self) -> bool {
        false
    }
}

/// The mock backend with `vision.mock_target` on: every model it loads emits
/// one synthetic `person` that orbits the frame centre, so detection consumers
/// (overlays, follow-me) can be exercised on a bench with no real model. It is
/// still not inference-capable: nothing it reports was measured.
#[derive(Debug, Default, Clone)]
pub struct MockTargetBackend;

/// Track id the synthetic target always carries.
pub const MOCK_TARGET_TRACK_ID: u64 = 1;
/// Orbit frequency of the synthetic target, in revolutions per second.
const MOCK_TARGET_HZ: f64 = 0.1;
/// Orbit radius, as a fraction of the frame.
const MOCK_TARGET_RADIUS: f64 = 0.25;
/// Box size, as a fraction of the frame width and height.
const MOCK_TARGET_W: f32 = 0.12;
const MOCK_TARGET_H: f32 = 0.3;

struct MockTargetModel {
    started: Instant,
}

/// The synthetic target at `t` seconds after the model loaded, in pixels of a
/// `width` x `height` frame. The centre orbits `(0.5, 0.5)` at radius 0.25.
pub fn mock_target_at(t: f64, width: u32, height: u32) -> Detection {
    let phase = 2.0 * std::f64::consts::PI * MOCK_TARGET_HZ * t;
    let cx = (0.5 + MOCK_TARGET_RADIUS * phase.cos()) as f32;
    let cy = (0.5 + MOCK_TARGET_RADIUS * phase.sin()) as f32;
    let (fw, fh) = (width as f32, height as f32);
    Detection {
        bbox: Some(BoundingBox {
            x: (cx - MOCK_TARGET_W / 2.0) * fw,
            y: (cy - MOCK_TARGET_H / 2.0) * fh,
            width: MOCK_TARGET_W * fw,
            height: MOCK_TARGET_H * fh,
        }),
        class_label: "person".into(),
        confidence: 0.9,
        track_id: Some(MOCK_TARGET_TRACK_ID),
        assoc_confidence: None,
        lock_state: None,
        attributes: None,
        mask: None,
        keypoints: None,
        depth: None,
        world_pos: None,
    }
}

impl LoadedModel for MockTargetModel {
    fn infer(&self, _frame: &[u8], w: u32, h: u32, _f: FrameFormat) -> Result<Vec<Detection>> {
        Ok(vec![mock_target_at(
            self.started.elapsed().as_secs_f64(),
            w,
            h,
        )])
    }
}

impl VisionBackend for MockTargetBackend {
    fn load(&self, _meta: &ModelMetadata) -> Result<Box<dyn LoadedModel>> {
        Ok(Box::new(MockTargetModel {
            started: Instant::now(),
        }))
    }
    fn name(&self) -> &str {
        "mock"
    }
    fn is_inference_capable(&self) -> bool {
        false
    }
}

/// The mock backend, with or without the synthetic target.
pub(super) fn mock_backend(mock_target: bool) -> Box<dyn VisionBackend> {
    if mock_target {
        Box::new(MockTargetBackend)
    } else {
        Box::new(MockBackend)
    }
}
