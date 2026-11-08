//! Moving the camera's track with the vehicle's turn.
//!
//! The replay is a recorded follow flight in a mine drift: the boxes the
//! person detector published per batch (about 3 Hz, 1280x720) from the
//! operator's designation of a walker on, and the autopilot's `ATTITUDE`
//! stream (20 Hz) over the same seconds. The front camera is fixed 15° down
//! with an 87° field of view. To follow, the vehicle pitches back and forth
//! by about 12°, which throws the walker up to 220 px up or down the image
//! between two batches while the walker moves a few tens of pixels.

use std::collections::HashMap;

use super::*;
use crate::depth::feed::Attitude;
use crate::depth::geometry::CameraGeometry;
use ados_protocol::framebus::{BoundingBox, DetectionHead, LockState, ModelKind};

#[path = "camera_motion_replay.rs"]
mod replay;

const W: u32 = 1280;
const H: u32 = 720;
/// A batch is published about this long after its frame was exposed; the
/// attitude history has caught up to then.
const PUBLISH_LAG_MS: u64 = 300;

fn person(x: f32, y: f32, width: f32, height: f32, confidence: f32) -> Detection {
    Detection {
        bbox: Some(BoundingBox {
            x,
            y,
            width,
            height,
        }),
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

/// The recorded detector: the frame carries its batch's index in its first
/// two bytes, and the model answers with the boxes published for it.
struct Recorded;

impl LoadedModel for Recorded {
    fn infer(&self, frame: &[u8], _: u32, _: u32, _: FrameFormat) -> Result<Vec<Detection>> {
        let i = usize::from(u16::from_le_bytes([frame[0], frame[1]]));
        Ok(replay::BATCHES[i]
            .2
            .iter()
            .map(|&(x, y, w, h, c)| person(x, y, w, h, c))
            .collect())
    }
}

struct RecordedBackend;

impl VisionBackend for RecordedBackend {
    fn load(&self, _: &ModelMetadata) -> Result<Box<dyn LoadedModel>> {
        Ok(Box::new(Recorded))
    }
    fn name(&self) -> &str {
        "recorded"
    }
    fn is_inference_capable(&self) -> bool {
        true
    }
}

/// A grey frame (so no crop pass runs: the replay holds what the flight's
/// detector published, crop included) stamped with simulation time.
fn frame(i: usize) -> (FrameDescriptor, Vec<u8>) {
    let (frame_id, sim_ms, ..) = replay::BATCHES[i];
    let mut pixels = vec![0u8; (W * H) as usize];
    pixels[..2].copy_from_slice(&(i as u16).to_le_bytes());
    let desc = FrameDescriptor {
        v: ados_protocol::framebus::FRAMEBUS_DESCRIPTOR_VERSION,
        camera_id: "main".into(),
        frame_id,
        // The engine's clock runs on its own origin; the simulation stamp is
        // the one the attitude is looked up by.
        ts_ns: (sim_ms + 9_000_000) * 1_000_000,
        ts_sensor_ns: Some(sim_ms * 1_000_000),
        exposure_ns: None,
        trigger_seq: Some(frame_id),
        timebase: ados_protocol::framebus::TIMEBASE_SIM,
        width: W,
        height: H,
        format: FrameFormat::Gray8,
        seq: frame_id,
        slot: 0,
        shm_name: String::new(),
        byte_len: W * H,
    };
    (desc, pixels)
}

async fn engine(moving: bool) -> Arc<VisionEngine> {
    let e =
        VisionEngine::with_tracker(Box::new(RecordedBackend), 4, true, TrackerConfig::default());
    e.register_model(ModelMetadata {
        id: "det".into(),
        kind: ModelKind::Detection,
        execution: ModelExecution::EngineRun,
        input_width: 640,
        input_height: 640,
        input_format: FrameFormat::Gray8,
        output_classes: vec!["person".into()],
        model_path: Some("/recorded".into()),
        head: DetectionHead::Yolo8,
    })
    .await
    .unwrap();
    if moving {
        e.configure_camera_motion(CameraMotionSetup {
            geometries: HashMap::from([(
                "main".to_string(),
                CameraGeometry {
                    hfov_deg: 87.0,
                    pitch_down_deg: 15.0,
                },
            )]),
            fc_on_sim_clock: true,
        });
    }
    e
}

/// One batch after the designation, as published.
#[derive(Debug)]
struct Flown {
    frame_id: u64,
    /// The walker was among the detector's boxes.
    seen: bool,
    /// The designated track's lock state.
    lock: Option<LockState>,
    /// How far the held box's centre is from the walker's true one, px, when
    /// the walker is in view (at least 80 px tall, unoccluded).
    miss_px: Option<f32>,
    /// On a batch the walker was in view but not among the detector's boxes:
    /// whether the crop window around the held box takes the whole walker in
    /// (the crop pass on the next batch looks there).
    crop_holds_walker: Option<bool>,
}

fn inside(b: &BoundingBox, (x, y, w, h): (f32, f32, f32, f32)) -> bool {
    x >= b.x && y >= b.y && x + w <= b.x + b.width && y + h <= b.y + b.height
}

async fn fly(e: &VisionEngine) -> Vec<Flown> {
    let feed = e.range_feed();
    let mut fed = 0;
    let mut feed_until = |sim_ms: u64| {
        while let Some(&(t, roll, pitch, yaw)) = replay::ATTITUDE.get(fed) {
            if u64::from(t) > sim_ms + PUBLISH_LAG_MS {
                break;
            }
            feed.push_attitude(u64::from(t), Attitude { roll, pitch, yaw }, u64::from(t));
            fed += 1;
        }
    };
    // The operator designates the walker on the first published batch.
    feed_until(replay::BATCHES[0].1);
    let (desc, pixels) = frame(0);
    let first = e.infer_and_publish("det", &desc, &pixels).await.unwrap();
    let walker = replay::BATCHES[0].3.unwrap();
    let id = e
        .designate("main", &first.detections[walker])
        .await
        .expect("designated");
    let mut out = Vec::new();
    let mut misses = 0;
    for i in 1..replay::BATCHES.len() {
        feed_until(replay::BATCHES[i].1);
        let (desc, pixels) = frame(i);
        let batch = e.infer_and_publish("det", &desc, &pixels).await.unwrap();
        let held = batch.detections.iter().find(|d| d.track_id == Some(id));
        let held_box = held.and_then(|d| d.bbox);
        let truth = replay::BATCHES[i].4.filter(|t| t.3 >= 80.0);
        let miss_px = held_box.zip(truth).map(|(b, (x, y, w, h))| {
            let (cx, cy) = (b.x + b.width / 2.0, b.y + b.height / 2.0);
            (cx - (x + w / 2.0)).hypot(cy - (y + h / 2.0))
        });
        let seen = replay::BATCHES[i].3.is_some();
        misses = if seen { 0 } else { misses + 1 };
        let crop_holds_walker = match (seen, held_box, truth) {
            (false, Some(b), Some(t)) => Some(
                super::crop::crop_window(&b, misses, W, H, 640, 640).is_some_and(|w| inside(&w, t)),
            ),
            _ => None,
        };
        out.push(Flown {
            frame_id: replay::BATCHES[i].0,
            seen,
            lock: held.and_then(|d| d.lock_state),
            miss_px,
            crop_holds_walker,
        });
    }
    out
}

/// Batches the walker was detected on but the lock was not `locked` (each a
/// follow-me hold), and batches the walker was in view, undetected, and
/// outside the crop window the lock's prediction placed.
fn flaps_and_crop_misses(run: &[Flown]) -> (Vec<u64>, Vec<u64>) {
    let flaps = run
        .iter()
        .filter(|b| b.seen && b.lock != Some(LockState::Locked))
        .map(|b| b.frame_id)
        .collect();
    let crop_misses = run
        .iter()
        .filter(|b| b.crop_holds_walker == Some(false))
        .map(|b| b.frame_id)
        .collect();
    (flaps, crop_misses)
}

/// The flight's failure, reproduced: without the camera's turn the lock
/// drops to uncertain on four batches the walker was detected on, each right
/// after a pitch swing (plus the designed re-confirm at 755, after the walker
/// left the detections for three batches); and once the walker leaves the
/// detections for good (812 on, while the vehicle yaws) the coast holds where
/// the walker was, so the crop window misses it, and the lock is lost.
#[tokio::test]
async fn without_the_turn_the_lock_flaps_and_the_crop_loses_the_walker() {
    let e = engine(false).await;
    let run = fly(&e).await;
    let (flaps, crop_misses) = flaps_and_crop_misses(&run);
    assert_eq!(flaps, [667, 677, 755, 788, 800], "{run:?}");
    assert_eq!(crop_misses, [812, 823, 833], "{run:?}");
    assert_eq!(run.last().unwrap().lock, Some(LockState::Lost));
    assert_eq!(e.turn_moves("main"), 0);
}

/// With the attitude, every batch the walker was detected on keeps it
/// locked but the designed re-confirm, and while the walker is in view but
/// undetected the moved prediction follows it (within 50 px) so the crop
/// window takes it in.
#[tokio::test]
async fn moved_with_the_turn_the_lock_holds_and_the_crop_follows_the_walker() {
    let e = engine(true).await;
    let run = fly(&e).await;
    let (flaps, crop_misses) = flaps_and_crop_misses(&run);
    assert_eq!(flaps, [755], "{run:?}");
    assert!(crop_misses.is_empty(), "{run:?}");
    for b in run.iter().filter(|b| !b.seen) {
        if let Some(px) = b.miss_px {
            assert!(px < 50.0, "frame {}: {px} px off the walker", b.frame_id);
        }
    }
    // Moved on every batch after the first.
    assert_eq!(e.turn_moves("main") as usize, run.len());
}

#[test]
fn a_simulated_frame_reads_the_attitude_by_its_simulation_stamp() {
    use crate::depth::feed::FrameTime;
    let (mut desc, _) = frame(0);
    let sim = desc.ts_sensor_ns.unwrap() as f64 / 1e6;
    let host = desc.ts_ns as f64 / 1e6;
    assert_eq!(
        camera_motion::frame_time(&desc, true),
        FrameTime::FlightController(sim)
    );
    // An autopilot on its own clock, or a frame stamped by another clock,
    // goes through the engine's clock.
    assert_eq!(
        camera_motion::frame_time(&desc, false),
        FrameTime::Host(host)
    );
    desc.timebase = ados_protocol::framebus::TIMEBASE_SENSOR;
    assert_eq!(
        camera_motion::frame_time(&desc, true),
        FrameTime::Host(host)
    );
}
