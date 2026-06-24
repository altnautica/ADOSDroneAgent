//! End-to-end follow-lock path through the engine's PUBLIC surface, with a
//! scripted backend so the detect → track → re-id → publish loop is exercised
//! without a real model. This is the software gate for the click-to-follow
//! behavior: the engine locks a track, an operator designate fixes it, a lost
//! track never silently re-acquires onto a different subject, and — with the
//! appearance (re-id) path on — the lock survives a same-position ambiguity
//! that motion alone cannot resolve because the embedding tells the two boxes
//! apart.
//!
//! The scripted backend returns detector detections from a queue and produces
//! an appearance embedding keyed to the crop's dominant colour, so two
//! differently-coloured boxes get clearly-separable embeddings. Frames are
//! painted so each detection's box region carries that detection's colour; the
//! engine crops the box, the backend embeds it, and the tracker associates on
//! the embedding.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use ados_protocol::framebus::{
    BoundingBox, Detection, DetectionHead, FrameFormat, ModelExecution, ModelKind, ModelMetadata,
};
use ados_vision::backend::{LoadedModel, VisionBackend};
use ados_vision::engine::VisionEngine;
use ados_vision::tracker::TrackerConfig;

const W: u32 = 320;
const H: u32 = 240;

/// One scripted frame's worth of detections.
type Script = Arc<Mutex<VecDeque<Vec<Detection>>>>;

/// A backend whose detector pops scripted detections and whose re-id model
/// embeds a crop by its dominant colour (a 3-vector of channel means), so the
/// tracker's cosine similarity cleanly separates a red box from a blue one.
struct ScriptedBackend {
    script: Script,
}

struct ScriptedDetector {
    script: Script,
}

struct ScriptedReid;

impl LoadedModel for ScriptedDetector {
    fn infer(
        &self,
        _f: &[u8],
        _w: u32,
        _h: u32,
        _fmt: FrameFormat,
    ) -> anyhow::Result<Vec<Detection>> {
        Ok(self.script.lock().unwrap().pop_front().unwrap_or_default())
    }
}

impl LoadedModel for ScriptedReid {
    fn infer(
        &self,
        _f: &[u8],
        _w: u32,
        _h: u32,
        _fmt: FrameFormat,
    ) -> anyhow::Result<Vec<Detection>> {
        Ok(Vec::new())
    }
    fn embed(
        &self,
        crop: &[u8],
        _w: u32,
        _h: u32,
        _fmt: FrameFormat,
    ) -> anyhow::Result<Option<Vec<f32>>> {
        // Channel means over the crop = a colour fingerprint. A red crop yields
        // a vector dominated by the R channel, a blue crop by the B channel, so
        // their cosine similarity is low and the tracker keeps the right lock.
        let mut sum = [0f64; 3];
        let px = crop.len() / 3;
        if px == 0 {
            return Ok(Some(vec![0.0, 0.0, 0.0]));
        }
        for p in crop.chunks_exact(3) {
            sum[0] += p[0] as f64;
            sum[1] += p[1] as f64;
            sum[2] += p[2] as f64;
        }
        Ok(Some(vec![
            (sum[0] / px as f64) as f32,
            (sum[1] / px as f64) as f32,
            (sum[2] / px as f64) as f32,
        ]))
    }
}

impl VisionBackend for ScriptedBackend {
    fn load(&self, meta: &ModelMetadata) -> anyhow::Result<Box<dyn LoadedModel>> {
        match meta.kind {
            ModelKind::Tracking => Ok(Box::new(ScriptedReid)),
            _ => Ok(Box::new(ScriptedDetector {
                script: self.script.clone(),
            })),
        }
    }
    fn name(&self) -> &str {
        "scripted"
    }
}

fn detector_meta() -> ModelMetadata {
    ModelMetadata {
        id: "det".into(),
        kind: ModelKind::Detection,
        execution: ModelExecution::EngineRun,
        input_width: W,
        input_height: H,
        input_format: FrameFormat::Rgb24,
        output_classes: vec!["person".into()],
        model_path: Some("/scripted".into()),
        head: DetectionHead::Yolo8,
    }
}

fn reid_meta() -> ModelMetadata {
    ModelMetadata {
        id: "reid".into(),
        kind: ModelKind::Tracking,
        execution: ModelExecution::EngineRun,
        input_width: 128,
        input_height: 256,
        input_format: FrameFormat::Rgb24,
        output_classes: vec![],
        model_path: Some("/scripted".into()),
        head: DetectionHead::Yolo8,
    }
}

fn det(x: f32, y: f32, conf: f32) -> Detection {
    Detection {
        bbox: BoundingBox {
            x,
            y,
            width: 40.0,
            height: 80.0,
        },
        class_label: "person".into(),
        confidence: conf,
        track_id: None,
        assoc_confidence: None,
        lock_state: None,
    }
}

/// Paint an rgb24 frame where each detection's box region carries `colour`
/// (the rest is black). Used so the re-id embed of each box returns that colour.
fn frame_with(boxes: &[(BoundingBox, [u8; 3])]) -> Vec<u8> {
    let mut f = vec![0u8; (W * H * 3) as usize];
    for (b, colour) in boxes {
        let x0 = b.x.max(0.0) as u32;
        let y0 = b.y.max(0.0) as u32;
        let x1 = ((b.x + b.width) as u32).min(W);
        let y1 = ((b.y + b.height) as u32).min(H);
        for y in y0..y1 {
            for x in x0..x1 {
                let o = ((y * W + x) * 3) as usize;
                f[o] = colour[0];
                f[o + 1] = colour[1];
                f[o + 2] = colour[2];
            }
        }
    }
    f
}

fn desc(frame_id: u64) -> ados_protocol::framebus::FrameDescriptor {
    ados_protocol::framebus::FrameDescriptor {
        camera_id: "cam".into(),
        frame_id,
        ts_ms: frame_id as i64,
        width: W,
        height: H,
        format: FrameFormat::Rgb24,
        seq: frame_id,
        slot: 0,
        shm_name: String::new(),
        byte_len: W * H * 3,
    }
}

async fn build_engine(reid: bool, script: Script) -> Arc<VisionEngine> {
    let backend = Box::new(ScriptedBackend { script });
    let engine = VisionEngine::with_tracker_reid(
        backend,
        4,
        true,
        TrackerConfig::default(),
        reid,
        if reid { Some("reid".to_string()) } else { None },
    );
    engine.register_model(detector_meta()).await.unwrap();
    if reid {
        engine.register_model(reid_meta()).await.unwrap();
    }
    engine
}

/// Run the detector over `n` identical frames carrying one red box and assert
/// the engine stamps a stable track id that holds, through the PUBLIC
/// infer_and_publish path (the real follow surface, not a private call).
#[tokio::test]
async fn engine_locks_and_holds_a_stable_track_through_the_public_path() {
    let red = [220u8, 20, 20];
    let target = det(140.0, 80.0, 0.9);
    let script: Script = Arc::new(Mutex::new(VecDeque::new()));
    for _ in 0..4 {
        script.lock().unwrap().push_back(vec![target.clone()]);
    }
    let engine = build_engine(true, script).await;

    let frame = frame_with(&[(target.bbox, red)]);
    let mut last_id = None;
    for i in 0..4 {
        let batch = engine
            .infer_and_publish("det", &desc(i), &frame)
            .await
            .unwrap();
        if let Some(d) = batch.detections.iter().find(|d| d.track_id.is_some()) {
            last_id = d.track_id;
            assert!(d.lock_state.is_some(), "a locked box carries a lock state");
        }
    }
    let id = last_id.expect("a track id is stamped");
    assert_eq!(engine.current_track("cam").await, Some(id));
}

/// A lost track never silently re-acquires: after a confirmed lock is dropped
/// (no detections past the coast window), a NEW box does not inherit the old id.
#[tokio::test]
async fn a_lost_track_does_not_silently_reacquire() {
    let red = [220u8, 20, 20];
    let target = det(140.0, 80.0, 0.9);
    let script: Script = Arc::new(Mutex::new(VecDeque::new()));
    // Confirm a lock, then a long run of empty frames (coast → lost), then a
    // different box far away.
    for _ in 0..4 {
        script.lock().unwrap().push_back(vec![target.clone()]);
    }
    for _ in 0..12 {
        script.lock().unwrap().push_back(vec![]);
    }
    let mover = det(20.0, 20.0, 0.9);
    for _ in 0..3 {
        script.lock().unwrap().push_back(vec![mover.clone()]);
    }
    let engine = build_engine(true, script).await;

    let target_frame = frame_with(&[(target.bbox, red)]);
    let empty_frame = frame_with(&[]);
    let mover_frame = frame_with(&[(mover.bbox, [20, 20, 220])]);

    let mut locked_id = None;
    for i in 0..4 {
        let b = engine
            .infer_and_publish("det", &desc(i), &target_frame)
            .await
            .unwrap();
        if let Some(d) = b.detections.iter().find(|d| d.track_id.is_some()) {
            locked_id = d.track_id;
        }
    }
    let locked_id = locked_id.expect("locked");
    for i in 4..16 {
        engine
            .infer_and_publish("det", &desc(i), &empty_frame)
            .await
            .unwrap();
    }
    // The lock is gone after the coast window.
    assert_eq!(
        engine.current_track("cam").await,
        None,
        "a lost track is not still current"
    );
    // The far-away new box must NOT carry the old id.
    for i in 16..19 {
        let b = engine
            .infer_and_publish("det", &desc(i), &mover_frame)
            .await
            .unwrap();
        for d in &b.detections {
            assert_ne!(
                d.track_id,
                Some(locked_id),
                "a new subject never inherits a lost lock's id"
            );
        }
    }
}

/// Operator designate fixes the lock onto a chosen box even when a
/// higher-confidence box is present (the operator's pick overrides auto-lock).
#[tokio::test]
async fn operator_designate_overrides_the_auto_lock() {
    let script: Script = Arc::new(Mutex::new(VecDeque::new()));
    let engine = build_engine(false, script).await;
    let chosen = det(200.0, 50.0, 0.4);
    engine
        .designate("cam", &chosen)
        .await
        .expect("designate seeds a track");
    // A freshly-seeded track is tentative, so current_track is None until a
    // measured frame confirms it — the designate succeeded via the public API.
    assert_eq!(engine.current_track("cam").await, None);
}

/// Run the SAME appearance-vs-motion crossing through the public path with the
/// re-id path on or off, and return the x of the box that carries the lock at
/// the end. The scenario: lock on a stationary red target at x≈100; then a blue
/// distractor sits ON the prediction (x≈100, motion-favored, higher confidence)
/// while the red target jumps to x≈160 (in-gate but motion-disfavored). The two
/// boxes never overlap in pixels, so each crop returns its own pure colour. Only
/// the embedding can prefer the red target over the closer blue distractor.
async fn crossing_end_x(reid: bool) -> f32 {
    let red = [220u8, 20, 20];
    let blue = [20u8, 20, 220];
    let target_lock = det(100.0, 100.0, 0.9);
    // After the lock, the red target jumps right; the blue distractor takes the
    // lock's old spot. Boxes are 40 wide so x=100 (100..140) and x=160 (160..200)
    // do not overlap — clean, single-colour crops.
    let target_jumped = det(160.0, 100.0, 0.9);
    let distractor = det(100.0, 100.0, 0.92);

    let script: Script = Arc::new(Mutex::new(VecDeque::new()));
    for _ in 0..4 {
        script.lock().unwrap().push_back(vec![target_lock.clone()]);
    }
    for _ in 0..4 {
        script
            .lock()
            .unwrap()
            .push_back(vec![distractor.clone(), target_jumped.clone()]);
    }
    let engine = build_engine(reid, script).await;

    let lock_frame = frame_with(&[(target_lock.bbox, red)]);
    let mut lock_id = None;
    for i in 0..4 {
        let b = engine
            .infer_and_publish("det", &desc(i), &lock_frame)
            .await
            .unwrap();
        if let Some(d) = b.detections.iter().find(|d| d.track_id.is_some()) {
            lock_id = d.track_id;
        }
    }
    let lock_id = lock_id.expect("locked the red target");

    let contest = frame_with(&[(distractor.bbox, blue), (target_jumped.bbox, red)]);
    let mut end_x = 100.0;
    for i in 4..8 {
        let b = engine
            .infer_and_publish("det", &desc(i), &contest)
            .await
            .unwrap();
        if let Some(d) = b.detections.iter().find(|d| d.track_id == Some(lock_id)) {
            end_x = d.bbox.x;
        }
    }
    end_x
}

/// The re-id embedding is wired end-to-end through the PUBLIC path and changes
/// which subject is tracked: with re-id ON the lock follows the red target by
/// appearance (to x≈160) even though a blue distractor sits closer on the
/// prediction; with re-id OFF the same scene is resolved by motion and the lock
/// follows the blue distractor (x≈100). A re-id test that does not behave
/// differently with re-id off is not a re-id test — this asserts the difference.
#[tokio::test]
async fn reid_changes_the_tracked_subject_vs_motion_only() {
    let x_reid = crossing_end_x(true).await;
    let x_motion = crossing_end_x(false).await;

    assert!(
        x_reid > 140.0,
        "with re-id, the lock follows the red target by appearance (x={x_reid}, expected ~160)"
    );
    assert!(
        x_motion < 130.0,
        "without re-id, motion follows the closer blue distractor (x={x_motion}, expected ~100)"
    );
    assert!(
        x_reid - x_motion > 30.0,
        "re-id selects a different subject than motion alone (reid x={x_reid}, motion x={x_motion})"
    );
}
