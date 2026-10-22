# ados-tracker

[![crates.io](https://img.shields.io/crates/v/ados-tracker.svg)](https://crates.io/crates/ados-tracker)
[![docs.rs](https://img.shields.io/docsrs/ados-tracker)](https://docs.rs/ados-tracker)
[![license](https://img.shields.io/crates/l/ados-tracker.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-blue.svg)](https://github.com/altnautica/ADOSDroneAgent/blob/main/crates/Cargo.toml)

**Single-object visual tracker for drones: Kalman motion model with appearance re-identification, YOLOv8 head decoding and OSNet embedding helpers.**

Object detectors are noisy: boxes jitter, frames drop, and a second object of the same class can wander into view. When a drone follows or frames one target, it needs one stable identity, not a fresh list of boxes every frame. This crate turns per-frame detections into a single locked track with a persistent id, coasts through short misses, and refuses to silently swap onto a lookalike. It also decodes raw YOLO output tensors and prepares re-identification crops, so a detector and an appearance model can feed it directly.

```rust
use ados_protocol::framebus::{BoundingBox, Detection, LockState};
use ados_tracker::tracker::{SingleObjectTracker, TrackState, TrackerConfig};

fn person(x: f32) -> Detection {
    Detection {
        bbox: Some(BoundingBox { x, y: 100.0, width: 40.0, height: 80.0 }),
        class_label: "person".into(),
        confidence: 0.9,
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

let mut tracker = SingleObjectTracker::new(TrackerConfig::default());
let mut last = None;
// The target drifts right by 3 px per frame.
for frame in 0..5 {
    last = Some(tracker.update(&[person(200.0 + 3.0 * frame as f32)]));
}
let update = last.unwrap();
assert_eq!(update.state, TrackState::Confirmed);
let reported = update.detection.expect("a confirmed track is reported");
assert_eq!(reported.track_id, update.track_id);
println!("track {:?}, lock {:?}", update.track_id, reported.lock_state);

// A missed frame coasts on the prediction and keeps the id.
let coast = tracker.update(&[]);
assert_eq!(coast.state, TrackState::Coasting);
assert_eq!(coast.track_id, update.track_id);
assert_ne!(coast.detection.and_then(|d| d.lock_state), Some(LockState::Locked));
```

## Highlights

- Pure and deterministic: no camera, socket or clock. The same input sequence always yields the same id assignment.
- Never-silent identity: when two candidates look alike, the lock goes `Uncertain` and raises a re-designation request instead of guessing.
- Appearance-first association with a pluggable [`AppearanceModel`](https://docs.rs/ados-tracker/latest/ados_tracker/tracker/trait.AppearanceModel.html); a colour/gradient histogram ships, a learned embedding plugs in through the same trait.
- YOLOv8/v11 and YOLOv5/v7 head decoding with class-agnostic non-maximum suppression.
- 63 tests, including adversarial suites that try to provoke identity swaps, locks onto noise and distractor capture.
- One dependency: [`ados-protocol`](https://crates.io/crates/ados-protocol) for the shared `Detection` type.

## How it works

The crate has three modules.

### [`tracker`](https://docs.rs/ados-tracker/latest/ados_tracker/tracker/index.html)

[`SingleObjectTracker`](https://docs.rs/ados-tracker/latest/ados_tracker/tracker/struct.SingleObjectTracker.html) takes the per-frame detections (the shared `Detection` type, never a parallel one), predicts where the locked object should be next with a constant-velocity Kalman model over `[cx, cy, w, h]` and their rates, associates the single best detection, and stamps a persistent `track_id`, `lock_state` and `assoc_confidence` on it. A brief miss is coasted on the prediction and keeps the id; only a real loss drops the lock, and a re-acquire then mints a fresh id.

The lifecycle is `Idle`, `Tentative` (too few hits to report), `Confirmed`, and `Coasting` (held on the prediction through a miss or an ambiguous frame, reported `Uncertain`). A track can be seeded automatically or by an explicit operator pick with [`designate`](https://docs.rs/ados-tracker/latest/ados_tracker/tracker/struct.SingleObjectTracker.html#method.designate); `TrackUpdate::operator_designated` tells the two apart, so a consumer can present only operator-picked tracks as locked.

Geometry alone cannot separate two overlapping boxes: a distractor riding the prediction fits the motion gate as well as the true target that maneuvered off it. So [`update_with_appearance`](https://docs.rs/ados-tracker/latest/ados_tracker/tracker/struct.SingleObjectTracker.html#method.update_with_appearance) takes [`Candidate`](https://docs.rs/ados-tracker/latest/ados_tracker/tracker/struct.Candidate.html)s that carry an appearance descriptor beside the detection. The tracker captures a template at confirm and updates it slowly, so it cannot drift onto a distractor. The descriptor travels only through `Candidate`, never on the wire detection.

Association is appearance-first. Among candidates that clear the absolute appearance gate, the one that looks most like the template wins; motion fit and detector confidence only break ties within an ambiguity margin of the best. When two admissible candidates fall inside that margin, the tracker holds on the prediction, reports `Uncertain`, lowers its confidence and sets [`needs_redesignation`](https://docs.rs/ados-tracker/latest/ados_tracker/tracker/struct.SingleObjectTracker.html#method.needs_redesignation). The one case vision cannot solve, a lone appearance-identical impostor while the true target is out of frame, is left to that operator backstop rather than a guess.

All thresholds live in [`TrackerConfig`](https://docs.rs/ados-tracker/latest/ados_tracker/tracker/struct.TrackerConfig.html).

### [`yolo`](https://docs.rs/ados-tracker/latest/ados_tracker/yolo/index.html)

[`decode`](https://docs.rs/ados-tracker/latest/ados_tracker/yolo/fn.decode.html) turns a raw output tensor into detections in source-frame pixels. It supports the transposed `[1, 4+nc, anchors]` YOLOv8/v11 head (no objectness) and the `[1, anchors, 5+nc]` YOLOv5/v7 head, transposes a head delivered the other way round automatically, scales boxes by `frame / input`, applies the confidence gate and runs class-agnostic NMS. [`preprocess_rgb24_nchw`](https://docs.rs/ados-tracker/latest/ados_tracker/yolo/fn.preprocess_rgb24_nchw.html) does the matching stretch resize into an NCHW float tensor.

### [`reid`](https://docs.rs/ados-tracker/latest/ados_tracker/reid/index.html)

Helpers for a re-identification model such as OSNet: crop and resize a detection box out of an rgb24 frame, normalize the crop with ImageNet statistics into an NCHW tensor, and L2-normalize the returned embedding so cosine similarity becomes a dot product.

## Part of ADOS

Used in production by the [ADOS Drone Agent](https://github.com/altnautica/ADOSDroneAgent), the onboard software of the ADOS software-defined drone platform.
[All ADOS crates](https://github.com/altnautica/ADOSDroneAgent/tree/main/crates#published-crates) · [Docs](https://docs.altnautica.com) · [Discord](https://discord.gg/uxbvuD4d5q)

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
