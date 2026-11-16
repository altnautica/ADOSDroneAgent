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
/// pixels it is handed (a box cut by the frame edge is not a person). It
/// counts its calls and logs the image size of each.
struct CliffDetector {
    calls: Arc<AtomicUsize>,
    log: Arc<std::sync::Mutex<Vec<(u32, u32)>>>,
}

impl LoadedModel for CliffDetector {
    fn infer(&self, frame: &[u8], w: u32, h: u32, f: FrameFormat) -> Result<Vec<Detection>> {
        assert_eq!(f, FrameFormat::Rgb24);
        assert_eq!(frame.len(), (w * h * 3) as usize);
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.log.lock().unwrap().push((w, h));
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
    log: Arc<std::sync::Mutex<Vec<(u32, u32)>>>,
}

/// A model that loads but fails every run (a crop model exported at an input
/// its config does not state).
struct BrokenDetector;

impl LoadedModel for BrokenDetector {
    fn infer(&self, _: &[u8], _: u32, _: u32, _: FrameFormat) -> Result<Vec<Detection>> {
        Err(anyhow!("input shape mismatch"))
    }
}

impl VisionBackend for CliffBackend {
    fn load(&self, meta: &ModelMetadata) -> Result<Box<dyn LoadedModel>> {
        Ok(match meta.kind {
            ModelKind::Tracking => Box::new(ColourReid),
            _ if meta.id.ends_with(".broken") => Box::new(BrokenDetector),
            _ => Box::new(CliffDetector {
                calls: self.calls.clone(),
                log: self.log.clone(),
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
    let (e, calls, _) = logged_follow_engine().await;
    (e, calls)
}

/// [`follow_engine`], with the image size of every detector call.
async fn logged_follow_engine() -> (
    Arc<VisionEngine>,
    Arc<AtomicUsize>,
    Arc<std::sync::Mutex<Vec<(u32, u32)>>>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let e = VisionEngine::with_tracker_reid(
        Box::new(CliffBackend {
            calls: calls.clone(),
            log: log.clone(),
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
    (e, calls, log)
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

/// The frame `frame_id` of a 3 Hz detector (the replays' batch rate).
fn desc(frame_id: u64) -> FrameDescriptor {
    desc_at(frame_id, 1_000_000_000 + frame_id * 333_333_333)
}

fn desc_at(frame_id: u64, ts_ns: u64) -> FrameDescriptor {
    FrameDescriptor {
        v: ados_protocol::framebus::FRAMEBUS_DESCRIPTOR_VERSION,
        camera_id: "main".into(),
        frame_id,
        ts_ns,
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
    /// The vertical scale of the whole-frame pass beside the crop.
    const WHOLE: Option<f32> = Some(MODEL_IN as f32 / H as f32);
    use super::crop::{crop_window, CROP_SUBJECT_MODEL_HEIGHT};
    // An 80 px subject: a 400 px square window puts it 128 px tall in a 640
    // input, centred on the box.
    let w = crop_window(
        &bbox([600.0, 300.0, 40.0, 80.0]),
        0.0,
        W,
        H,
        MODEL_IN,
        MODEL_IN,
        WHOLE,
    )
    .unwrap();
    assert!(
        (w.width - 400.0).abs() < 0.01 && (w.height - 400.0).abs() < 0.01,
        "{w:?}"
    );
    assert!((80.0 * MODEL_IN as f32 / w.height - CROP_SUBJECT_MODEL_HEIGHT).abs() < 0.01);
    assert_eq!((w.x, w.y), (420.0, 140.0));

    // Near a corner the window moves inside the frame instead of shrinking.
    let w = crop_window(
        &bbox([2.0, 650.0, 30.0, 60.0]),
        0.0,
        W,
        H,
        MODEL_IN,
        MODEL_IN,
        WHOLE,
    )
    .unwrap();
    assert_eq!((w.x, w.y + w.height), (0.0, H as f32));

    // A tiny subject is magnified at most CROP_MAX_UPSCALE.
    let w = crop_window(
        &bbox([600.0, 300.0, 4.0, 8.0]),
        0.0,
        W,
        H,
        MODEL_IN,
        MODEL_IN,
        WHOLE,
    )
    .unwrap();
    assert!((w.height - MODEL_IN as f32 / super::crop::CROP_MAX_UPSCALE).abs() < 0.01);

    // A measured subject the whole-frame pass already sees at the target size:
    // no crop beside a whole-frame pass.
    let large = bbox([600.0, 300.0, 70.0, 150.0]);
    assert!(crop_window(&large, 0.0, W, H, MODEL_IN, MODEL_IN, WHOLE).is_none());
    // Without one the crop is the only pass, so it runs whatever the size.
    assert!(crop_window(&large, 0.0, W, H, MODEL_IN, MODEL_IN, None).is_some());
    // The same box after two steps without a measurement is a stale size for
    // a subject that is receding: the crop runs, scaled for the shrunken
    // subject.
    let w = crop_window(&large, 2.0, W, H, MODEL_IN, MODEL_IN, WHOLE).unwrap();
    let expected = 150.0 * super::crop::CROP_COAST_SHRINK.powi(2);
    assert!((expected * MODEL_IN as f32 / w.height - CROP_SUBJECT_MODEL_HEIGHT).abs() < 0.01);
    // A non-square input keeps its aspect.
    let w = crop_window(
        &bbox([600.0, 300.0, 40.0, 80.0]),
        0.0,
        W,
        H,
        640,
        384,
        WHOLE,
    )
    .unwrap();
    assert!((w.width / w.height - 640.0 / 384.0).abs() < 0.01);
}

/// The walker receding per 100 ms frame, at the 10 Hz the detector now runs
/// at: [`WALK_AWAY`]'s sizes and drift interpolated, without the camera's
/// pitch throws (nothing here moves the track with the camera).
fn walk_at_10_hz() -> Vec<[f32; 4]> {
    let mut out = Vec::new();
    for pair in WALK_AWAY.windows(2) {
        for k in 0..3 {
            let t = k as f32 / 3.0;
            let mut b: [f32; 4] =
                std::array::from_fn(|i| pair[0][i] + (pair[1][i] - pair[0][i]) * t);
            b[1] = 300.0;
            out.push(b);
        }
    }
    out
}

/// While a designated subject is tracked, the detector's batches run only the
/// crop around it, and the whole frame twice a second: at 10 Hz, one batch in
/// five carries a whole-frame pass (with the crop beside it only once the
/// crop magnifies the walker), and the walker stays locked.
#[tokio::test]
async fn crop_primary_runs_the_whole_frame_twice_a_second() {
    let (e, _, log) = logged_follow_engine().await;
    e.configure_crop_primary("det", None, 500_000_000);
    let walk = walk_at_10_hz();
    let at = |i: usize| desc_at(i as u64, 1_000_000_000 + i as u64 * 100_000_000);
    let first = e
        .infer_and_publish("det", &at(0), &frame_with(&bbox(walk[0])))
        .await
        .unwrap();
    let id = e.designate("main", &first.detections[0]).await.unwrap();
    log.lock().unwrap().clear();
    let mut locked = 0;
    let mut windowed = 0;
    let frames = 20;
    for (i, b) in walk.iter().enumerate().skip(1).take(frames) {
        let batch = e
            .infer_and_publish("det", &at(i), &frame_with(&bbox(*b)))
            .await
            .unwrap();
        locked += batch
            .detections
            .iter()
            .filter(|d| d.track_id == Some(id) && d.lock_state == Some(LockState::Locked))
            .count();
        // A crop-only batch names the window it looked in, around the walker.
        if let Some(w) = batch.crop_window {
            windowed += 1;
            let walker = bbox(*b);
            assert!(
                w.x <= walker.x && w.x + w.width >= walker.x + walker.width,
                "frame {i}: window {w:?} misses the walker {walker:?}"
            );
        }
    }
    let calls = log.lock().unwrap().clone();
    let whole = calls.iter().filter(|s| **s == (W, H)).count();
    let crops = calls.iter().filter(|s| **s == (MODEL_IN, MODEL_IN)).count();
    assert_eq!(calls.len(), whole + crops, "{calls:?}");
    // The whole frame ran every fifth batch (its first run is due half a
    // second after the designating batch's).
    assert_eq!(whole, 4, "{whole} whole-frame passes in 2 s");
    assert!(crops >= frames - whole, "{crops} crops in {frames} batches");
    assert_eq!(
        windowed,
        frames - whole,
        "every crop-only batch names its window"
    );
    assert!(
        locked >= frames - 1,
        "locked on {locked} of {frames} batches"
    );
    let status = e.detection_status(&["main".to_string()], 0);
    let b = &status.cameras["main"].batches;
    assert_eq!(b.batches, frames as u64 + 1);
    assert_eq!(b.full_frame_runs, whole as u64 + 1);
}

/// A crop model of its own runs the crop pass at its own input size: the
/// window carries the magnification, so a 320 px crop holds the walker.
#[tokio::test]
async fn a_crop_model_runs_the_crop_at_its_own_input() {
    let (e, _, log) = logged_follow_engine().await;
    e.register_model(model("det.crop", ModelKind::Detection, 320, 320))
        .await
        .unwrap();
    e.configure_crop_primary("det", Some("det.crop".into()), 500_000_000);
    let (id, frames) = replay(&e, &WALK_AWAY[..6]).await;
    assert!(log.lock().unwrap().contains(&(320, 320)));
    assert!(!log.lock().unwrap().contains(&(MODEL_IN, MODEL_IN)));
    let (state, b) = frames[2];
    assert_eq!(state, Some(LockState::Locked), "track {id}");
    assert!(iou(&b, &bbox(WALK_AWAY[3])) > 0.5);
    // The crop model reports its own latency, and the detector's is the
    // batch's inference time: a crop-only batch counts its crop, not 0.
    let models = e.list_models().await;
    let latency = |id: &str| models.iter().find(|m| m.id == id).unwrap().latency_ms;
    assert!(latency("det.crop") > 0.0);
    assert!(
        latency("det") >= 0.9 * latency("det.crop"),
        "detector {} ms, crop {} ms",
        latency("det"),
        latency("det.crop")
    );
}

/// A designation asks the capture loop for a batch on the next frame, once.
#[tokio::test]
async fn a_designation_is_taken_once() {
    let (e, _) = follow_engine().await;
    assert!(!e.take_designation("main"));
    e.designate("main", &person(bbox(WALK_AWAY[0]), 0.9))
        .await
        .unwrap();
    assert!(e.take_designation("main"));
    assert!(!e.take_designation("main"));
}

/// The tracker coasts in seconds: at 10 Hz a walker hidden for 2 s is still
/// held (uncertain), where a coast counted in batches (8) would have dropped
/// it after 0.9 s; hidden for 3 s it is lost.
#[tokio::test]
async fn a_ten_hz_lock_coasts_as_long_as_a_three_hz_one() {
    let (e, _) = follow_engine().await;
    let walker = bbox([617.0, 314.0, 67.0, 136.0]);
    let at = |i: u64| desc_at(i, 1_000_000_000 + i * 100_000_000);
    let first = e
        .infer_and_publish("det", &at(0), &frame_with(&walker))
        .await
        .unwrap();
    let id = e.designate("main", &first.detections[0]).await.unwrap();
    e.infer_and_publish("det", &at(1), &frame_with(&walker))
        .await
        .unwrap();
    let empty = BACKGROUND.repeat((W * H) as usize);
    let mut state_at = Vec::new();
    for i in 2..=32u64 {
        let batch = e.infer_and_publish("det", &at(i), &empty).await.unwrap();
        let s = batch
            .detections
            .iter()
            .find(|d| d.track_id == Some(id))
            .and_then(|d| d.lock_state);
        state_at.push((i, s));
    }
    // Hidden from frame 2: at frame 21 (2.0 s) held, by frame 32 (3.1 s) lost.
    assert!(state_at
        .iter()
        .filter(|(i, _)| *i <= 21)
        .all(|(_, s)| *s == Some(LockState::Uncertain)));
    assert!(state_at.iter().any(|(_, s)| *s == Some(LockState::Lost)));
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

/// A crop model that loads but fails every run costs only the crop: each
/// batch it would have run alone runs the whole frame instead, so no batch
/// reads as an empty window and the lock holds.
#[tokio::test]
async fn a_crop_model_that_fails_every_run_falls_back_to_the_whole_frame() {
    let (e, _) = follow_engine().await;
    e.register_model(model("det.broken", ModelKind::Detection, 320, 320))
        .await
        .unwrap();
    e.configure_crop_primary("det", Some("det.broken".into()), 500_000_000);
    let at = |i: u64| desc_at(i, 1_000_000_000 + i * 100_000_000);
    let walker = |i: u64| bbox([600.0 + i as f32 * 4.0, 300.0, 70.0, 200.0]);
    let first = e
        .infer_and_publish("det", &at(0), &frame_with(&walker(0)))
        .await
        .unwrap();
    let id = e.designate("main", &first.detections[0]).await.unwrap();
    for i in 1..12 {
        let batch = e
            .infer_and_publish("det", &at(i), &frame_with(&walker(i)))
            .await
            .unwrap();
        assert_eq!(batch.crop_window, None, "frame {i}: {batch:?}");
        if i >= 2 {
            assert!(
                batch
                    .detections
                    .iter()
                    .any(|d| d.track_id == Some(id) && d.lock_state == Some(LockState::Locked)),
                "frame {i}: {batch:?}"
            );
        }
    }
}
