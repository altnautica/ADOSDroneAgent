//! The detector pass on a crop around the designated track.
//!
//! The replay is a person walking away from a camera at the follow standoff:
//! the subject's box per detector batch, 1280x720, shrinking from 136 px to
//! 81 px tall over about 5 s while the camera's pitch throws it up and down by
//! up to 190 px. The detector is a stand-in with the measured recall cliff of
//! a person detector run on the whole frame: it finds the subject only when
//! the subject is at least `MIN_MODEL_HEIGHT` pixels tall in the model's input
//! (124 px in a 720 px frame squeezed into a 640 px input). It finds the
//! subject in the pixels it is handed, so a crop that magnifies the subject
//! brings it back above the cliff and a crop that cuts it misses it.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use ados_protocol::framebus::{BoundingBox, Detection, DetectionHead, LockState, ModelKind};

const W: u32 = 1280;
const H: u32 = 720;
/// The detector's square input.
const MODEL_IN: u32 = 640;
/// Smallest subject height, in model-input pixels, the stand-in detector finds.
const MIN_MODEL_HEIGHT: f32 = 110.0;
const BACKGROUND: [u8; 3] = [70, 95, 60];
const WALKER: [u8; 3] = [200, 40, 40];

/// The walker's box `[x, y, w, h]` per detector batch, from the frame the
/// subject was designated on.
const WALK_AWAY: [[f32; 4]; 16] = [
    [617.0, 314.0, 67.0, 136.0],
    [620.0, 306.0, 64.0, 130.0],
    [624.0, 295.0, 61.0, 121.0],
    [623.0, 281.0, 58.0, 119.0],
    [618.0, 119.0, 58.0, 120.0],
    [620.0, 227.0, 55.0, 113.0],
    [619.0, 422.0, 54.0, 110.0],
    [623.0, 333.0, 51.0, 105.0],
    [623.0, 238.0, 49.0, 101.0],
    [625.0, 258.0, 47.0, 98.0],
    [627.0, 251.0, 46.0, 91.0],
    [629.0, 246.0, 44.0, 90.0],
    [634.0, 236.0, 43.0, 87.0],
    [638.0, 78.0, 43.0, 94.0],
    [640.0, 116.0, 42.0, 88.0],
    [642.0, 341.0, 40.0, 81.0],
];

fn person(b: BoundingBox, confidence: f32) -> Detection {
    Detection {
        bbox: Some(b),
        class_label: "person".into(),
        confidence,
        track_id: None,
        assoc_confidence: None,
        lock_state: None,
        attributes: None,
        mask: None,
        keypoints: None,
        depth: None,
        world_pos: None,
    }
}

fn bbox([x, y, width, height]: [f32; 4]) -> BoundingBox {
    BoundingBox {
        x,
        y,
        width,
        height,
    }
}

/// The bounding box of the walker-coloured pixels in an rgb24 frame. A
/// resampled crop blends the walker's edge into the background, so the test is
/// a threshold, not an exact colour.
fn walker_in(frame: &[u8], w: u32, h: u32) -> Option<BoundingBox> {
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
    for y in 0..h {
        for x in 0..w {
            let o = ((y * w + x) * 3) as usize;
            if frame[o] > 135 && frame[o + 1] < 70 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x + 1);
                y1 = y1.max(y + 1);
            }
        }
    }
    (x0 < x1).then(|| BoundingBox {
        x: x0 as f32,
        y: y0 as f32,
        width: (x1 - x0) as f32,
        height: (y1 - y0) as f32,
    })
}

/// A person detector with a recall cliff: it reports the walker only when the
/// walker, stretched into the model's input like the real preprocessing does,
/// is at least `MIN_MODEL_HEIGHT` tall, and only when the walker is whole in the
/// pixels it is handed (a box cut by the frame edge is not a person).
struct CliffDetector {
    calls: Arc<AtomicUsize>,
}

impl LoadedModel for CliffDetector {
    fn infer(&self, frame: &[u8], w: u32, h: u32, f: FrameFormat) -> Result<Vec<Detection>> {
        assert_eq!(f, FrameFormat::Rgb24);
        assert_eq!(frame.len(), (w * h * 3) as usize);
        self.calls.fetch_add(1, Ordering::SeqCst);
        let Some(b) = walker_in(frame, w, h) else {
            return Ok(Vec::new());
        };
        let cut =
            b.x <= 0.0 || b.y <= 0.0 || b.x + b.width >= w as f32 || b.y + b.height >= h as f32;
        let model_height = b.height * MODEL_IN as f32 / h as f32;
        if cut || model_height < MIN_MODEL_HEIGHT {
            return Ok(Vec::new());
        }
        Ok(vec![person(b, 0.7)])
    }
}

/// A re-id model that embeds a crop by its mean colour.
struct ColourReid;

impl LoadedModel for ColourReid {
    fn infer(&self, _: &[u8], _: u32, _: u32, _: FrameFormat) -> Result<Vec<Detection>> {
        Ok(Vec::new())
    }
    fn embed(&self, crop: &[u8], _: u32, _: u32, _: FrameFormat) -> Result<Option<Vec<f32>>> {
        let (px, _) = crop.as_chunks::<3>();
        let n = px.len().max(1) as f32;
        let mut sum = [0f32; 3];
        for p in px {
            for c in 0..3 {
                sum[c] += p[c] as f32;
            }
        }
        Ok(Some(sum.iter().map(|s| s / n).collect()))
    }
}

struct CliffBackend {
    calls: Arc<AtomicUsize>,
}

impl VisionBackend for CliffBackend {
    fn load(&self, meta: &ModelMetadata) -> Result<Box<dyn LoadedModel>> {
        Ok(match meta.kind {
            ModelKind::Tracking => Box::new(ColourReid),
            _ => Box::new(CliffDetector {
                calls: self.calls.clone(),
            }),
        })
    }
    fn name(&self) -> &str {
        "cliff"
    }
    fn is_inference_capable(&self) -> bool {
        true
    }
}

fn model(id: &str, kind: ModelKind, w: u32, h: u32) -> ModelMetadata {
    ModelMetadata {
        id: id.into(),
        kind,
        execution: ModelExecution::EngineRun,
        input_width: w,
        input_height: h,
        input_format: FrameFormat::Rgb24,
        output_classes: vec!["person".into()],
        model_path: Some("/cliff".into()),
        head: DetectionHead::Yolo8,
    }
}

/// The engine as a follow deployment runs it: tracker and re-id on.
async fn follow_engine() -> (Arc<VisionEngine>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let e = VisionEngine::with_tracker_reid(
        Box::new(CliffBackend {
            calls: calls.clone(),
        }),
        4,
        true,
        TrackerConfig::default(),
        true,
        Some("reid".into()),
        DetectionPolicy::default(),
    );
    e.register_model(model("det", ModelKind::Detection, MODEL_IN, MODEL_IN))
        .await
        .unwrap();
    e.register_model(model("reid", ModelKind::Tracking, 128, 256))
        .await
        .unwrap();
    (e, calls)
}

fn frame_with(walker: &BoundingBox) -> Vec<u8> {
    let mut f = BACKGROUND.repeat((W * H) as usize);
    let (x0, y0) = (walker.x as u32, walker.y as u32);
    let (x1, y1) = (
        ((walker.x + walker.width) as u32).min(W),
        ((walker.y + walker.height) as u32).min(H),
    );
    for y in y0..y1 {
        for x in x0..x1 {
            let o = ((y * W + x) * 3) as usize;
            f[o..o + 3].copy_from_slice(&WALKER);
        }
    }
    f
}

fn desc(frame_id: u64) -> FrameDescriptor {
    FrameDescriptor {
        v: ados_protocol::framebus::FRAMEBUS_DESCRIPTOR_VERSION,
        camera_id: "main".into(),
        frame_id,
        ts_ns: frame_id,
        ts_sensor_ns: None,
        exposure_ns: None,
        trigger_seq: None,
        timebase: ados_protocol::framebus::TIMEBASE_HOST,
        width: W,
        height: H,
        format: FrameFormat::Rgb24,
        seq: frame_id,
        slot: 0,
        shm_name: String::new(),
        byte_len: W * H * 3,
        hfov_deg: None,
        mount_pitch_deg: None,
    }
}

fn iou(a: &BoundingBox, b: &BoundingBox) -> f32 {
    super::tracking::bbox_iou(a, b)
}

/// Designate the walker on the first frame of `walk`, as the operator does
/// from the published batch, and replay the rest. Returns the designated id
/// and, per later frame, the designated detection's lock state and box.
async fn replay(
    e: &VisionEngine,
    walk: &[[f32; 4]],
) -> (u64, Vec<(Option<LockState>, BoundingBox)>) {
    let first = e
        .infer_and_publish("det", &desc(0), &frame_with(&bbox(walk[0])))
        .await
        .unwrap();
    let clicked = first.detections[0].clone();
    let id = e.designate("main", &clicked).await.expect("designated");
    let mut out = Vec::new();
    for (i, b) in walk.iter().enumerate().skip(1) {
        let batch = e
            .infer_and_publish("det", &desc(i as u64), &frame_with(&bbox(*b)))
            .await
            .unwrap();
        let held = batch.detections.iter().find(|d| d.track_id == Some(id));
        out.push(match held {
            Some(d) => (d.lock_state, d.bbox.unwrap()),
            None => (None, bbox([0.0; 4])),
        });
    }
    (id, out)
}

/// The stand-in reproduces the failure: on the whole frame alone the walker is
/// gone from the third batch on, so the designated lock coasts and is lost.
#[tokio::test]
async fn whole_frame_detection_loses_the_small_walker() {
    let (e, _) = follow_engine().await;
    let first = e
        .infer_and_publish("det", &desc(0), &frame_with(&bbox(WALK_AWAY[0])))
        .await
        .unwrap();
    let id = e.designate("main", &first.detections[0]).await.unwrap();
    let mut whole_frame_hits = 0;
    let mut lost = false;
    for b in WALK_AWAY.iter().skip(1) {
        // The whole-frame pass alone: the detector's own output, tracked.
        let frame = frame_with(&bbox(*b));
        let dets = e
            .infer("det", &frame, W, H, FrameFormat::Rgb24)
            .await
            .unwrap();
        whole_frame_hits += dets.len();
        let tracked = e
            .apply_tracker("main", dets, &frame, W, H, FrameFormat::Rgb24)
            .await;
        lost |= tracked
            .iter()
            .any(|d| d.track_id == Some(id) && d.lock_state == Some(LockState::Lost));
    }
    assert_eq!(
        whole_frame_hits, 1,
        "only the 130 px frame clears the cliff"
    );
    assert!(lost, "the coast runs out and the lock is lost");
}

/// With the crop pass the designated walker stays locked through the whole
/// walk: never lost, the same id on every batch, and every locked box on the
/// walker. The batches where the camera's pitch throws the walker 240 px from
/// the prediction (6 and 15) are beyond the tracker's association gate whatever
/// the detector finds, so those coast (uncertain), and 7 is the re-confirm
/// frame out of that coast.
#[tokio::test]
async fn crop_pass_keeps_the_small_walker_locked() {
    let (e, _) = follow_engine().await;
    let (id, frames) = replay(&e, &WALK_AWAY).await;
    assert_eq!(frames.len(), WALK_AWAY.len() - 1);
    let mut locked = 0;
    for (i, (state, b)) in frames.iter().enumerate() {
        let truth = bbox(WALK_AWAY[i + 1]);
        assert!(
            matches!(state, Some(LockState::Locked | LockState::Uncertain)),
            "frame {}: designated track {id} is {state:?}",
            i + 1
        );
        if *state == Some(LockState::Locked) {
            locked += 1;
            assert!(
                iou(b, &truth) > 0.5,
                "frame {}: locked box {b:?} is off the walker {truth:?}",
                i + 1
            );
        }
    }
    assert_eq!(locked, 12, "locked on {locked} of {} batches", frames.len());
    // The smallest walker (88 px, well below the 124 px cliff) is still locked.
    assert_eq!(frames[13].0, Some(LockState::Locked));
}

/// The walker is designated at 142 px, where the whole-frame pass still sees
/// it, and at the next batch is already below the whole-frame cliff. The
/// coasting track holds the 142 px box, and a crop scaled for 142 px would not
/// magnify past the whole frame, so the crop must assume the unmeasured
/// subject is shrinking or it never runs and the lock is lost.
#[tokio::test]
async fn crop_pass_finds_a_receding_walker_the_coast_holds_too_large() {
    let walk = [
        [607.0, 245.0, 65.0, 142.0],
        [608.0, 253.0, 59.0, 118.0],
        [608.0, 235.0, 56.0, 116.0],
        [610.0, 229.0, 54.0, 109.0],
        [613.0, 232.0, 52.0, 107.0],
        [617.0, 214.0, 50.0, 102.0],
        [618.0, 222.0, 49.0, 99.0],
        [616.0, 212.0, 47.0, 97.0],
        [619.0, 221.0, 46.0, 93.0],
        [619.0, 181.0, 44.0, 93.0],
        [618.0, 223.0, 43.0, 86.0],
    ];
    let (e, _) = follow_engine().await;
    let (id, frames) = replay(&e, &walk).await;
    for (i, (state, _)) in frames.iter().enumerate() {
        assert!(
            matches!(state, Some(LockState::Locked | LockState::Uncertain)),
            "frame {}: designated track {id} is {state:?}",
            i + 1
        );
    }
    let (state, b) = frames.last().unwrap();
    assert_eq!(*state, Some(LockState::Locked));
    assert!(iou(b, &bbox(walk[10])) > 0.5, "on the walker: {b:?}");
}

/// The crop pass costs a second inference only while a designated track is
/// live and the crop magnifies it; never on an undesignated stream.
#[tokio::test]
async fn crop_pass_runs_only_while_designated() {
    let (e, calls) = follow_engine().await;
    let small = frame_with(&bbox(WALK_AWAY[1]));
    let batch = e.infer_and_publish("det", &desc(0), &small).await.unwrap();
    e.infer_and_publish("det", &desc(1), &small).await.unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "no designation: one pass per frame"
    );

    e.designate("main", &batch.detections[0]).await.unwrap();
    e.infer_and_publish("det", &desc(2), &small).await.unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        4,
        "designated: whole frame plus crop"
    );

    e.reset_track("main").await;
    e.infer_and_publish("det", &desc(3), &small).await.unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        5,
        "designation dropped: no crop"
    );

    // A subject the whole-frame pass already sees large gets no crop: a crop
    // cannot magnify it past the whole frame's own scale.
    let big = person(bbox([500.0, 200.0, 120.0, 300.0]), 0.9);
    e.designate("main", &big).await.unwrap();
    e.infer_and_publish("det", &desc(4), &frame_with(big.bbox.as_ref().unwrap()))
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 6, "large subject: no crop");

    // The status sidecar counts the one crop pass, which found the walker.
    let status = e.detection_status(&["main".to_string()], 0);
    let main = &status.cameras["main"];
    assert_eq!((main.crop_runs, main.crop_hits), (1, 1));
}

#[test]
fn crop_window_scales_the_subject_to_the_target_height() {
    use super::crop::{crop_window, CROP_SUBJECT_MODEL_HEIGHT};
    // An 80 px subject: a 400 px square window puts it 128 px tall in a 640
    // input, centred on the box.
    let w = crop_window(
        &bbox([600.0, 300.0, 40.0, 80.0]),
        0,
        W,
        H,
        MODEL_IN,
        MODEL_IN,
    )
    .unwrap();
    assert!(
        (w.width - 400.0).abs() < 0.01 && (w.height - 400.0).abs() < 0.01,
        "{w:?}"
    );
    assert!((80.0 * MODEL_IN as f32 / w.height - CROP_SUBJECT_MODEL_HEIGHT).abs() < 0.01);
    assert_eq!((w.x, w.y), (420.0, 140.0));

    // Near a corner the window moves inside the frame instead of shrinking.
    let w = crop_window(&bbox([2.0, 650.0, 30.0, 60.0]), 0, W, H, MODEL_IN, MODEL_IN).unwrap();
    assert_eq!((w.x, w.y + w.height), (0.0, H as f32));

    // A tiny subject is magnified at most CROP_MAX_UPSCALE.
    let w = crop_window(&bbox([600.0, 300.0, 4.0, 8.0]), 0, W, H, MODEL_IN, MODEL_IN).unwrap();
    assert!((w.height - MODEL_IN as f32 / super::crop::CROP_MAX_UPSCALE).abs() < 0.01);

    // A measured subject the whole-frame pass already sees at the target size:
    // no crop.
    let large = bbox([600.0, 300.0, 70.0, 150.0]);
    assert!(crop_window(&large, 0, W, H, MODEL_IN, MODEL_IN).is_none());
    // The same box after two missed batches is a stale size for a subject
    // that is receding: the crop runs, scaled for the shrunken subject.
    let w = crop_window(&large, 2, W, H, MODEL_IN, MODEL_IN).unwrap();
    let expected = 150.0 * super::crop::CROP_COAST_SHRINK.powi(2);
    assert!((expected * MODEL_IN as f32 / w.height - CROP_SUBJECT_MODEL_HEIGHT).abs() < 0.01);
    // A non-square input keeps its aspect.
    let w = crop_window(&bbox([600.0, 300.0, 40.0, 80.0]), 0, W, H, 640, 384).unwrap();
    assert!((w.width / w.height - 640.0 / 384.0).abs() < 0.01);
}

#[test]
fn crop_detections_map_back_and_drop_cut_or_other_class() {
    use super::crop::crop_to_frame;
    let window = bbox([420.0, 140.0, 400.0, 400.0]);
    let whole = person(bbox([288.0, 256.0, 64.0, 128.0]), 0.8);
    let cut = person(bbox([0.5, 200.0, 40.0, 100.0]), 0.8);
    let mut other = person(bbox([100.0, 100.0, 50.0, 50.0]), 0.9);
    other.class_label = "car".into();
    let out = crop_to_frame(
        vec![whole, cut, other],
        &window,
        W,
        H,
        MODEL_IN,
        MODEL_IN,
        "person",
    );
    assert_eq!(out.len(), 1);
    let b = out[0].bbox.unwrap();
    assert_eq!((b.x, b.y, b.width, b.height), (600.0, 300.0, 40.0, 80.0));

    // On a window edge that is the frame's edge a box touching it is whole.
    let at_frame_edge = bbox([0.0, 140.0, 400.0, 400.0]);
    let edge = person(bbox([0.0, 200.0, 40.0, 100.0]), 0.8);
    assert_eq!(
        crop_to_frame(
            vec![edge],
            &at_frame_edge,
            W,
            H,
            MODEL_IN,
            MODEL_IN,
            "person"
        )
        .len(),
        1
    );
}

#[test]
fn merge_keeps_one_box_per_subject() {
    use super::crop::merge_crop;
    let mut dets = vec![person(bbox([600.0, 300.0, 40.0, 80.0]), 0.5)];
    // The same subject from the crop, more confident: it replaces the
    // whole-frame box. A second subject joins.
    merge_crop(
        &mut dets,
        vec![
            person(bbox([602.0, 302.0, 38.0, 78.0]), 0.7),
            person(bbox([900.0, 300.0, 40.0, 80.0]), 0.6),
        ],
    );
    assert_eq!(dets.len(), 2);
    assert_eq!(dets[0].confidence, 0.7);
    // A less confident duplicate is dropped.
    merge_crop(
        &mut dets,
        vec![person(bbox([601.0, 301.0, 40.0, 80.0]), 0.1)],
    );
    assert_eq!(dets.len(), 2);
    assert_eq!(dets[0].confidence, 0.7);
}
