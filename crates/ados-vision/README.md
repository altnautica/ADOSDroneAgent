# ados-vision

[![crates.io](https://img.shields.io/crates/v/ados-vision.svg)](https://crates.io/crates/ados-vision)
[![docs.rs](https://img.shields.io/docsrs/ados-vision)](https://docs.rs/ados-vision)
[![license](https://img.shields.io/crates/l/ados-vision.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-blue.svg)](https://github.com/altnautica/ADOSDroneAgent/blob/main/crates/Cargo.toml)

**Single-object visual tracker for drones: Kalman motion model with appearance re-identification, YOLOv8 head decoding and OSNet embedding helpers.**

Object detectors are noisy: boxes jitter, frames drop, and a second object of the same class can wander into view. When a drone follows or frames one target, it needs one stable identity, not a fresh list of boxes every frame. This crate turns per-frame detections into a single locked track with a persistent id, coasts through short misses, and refuses to silently swap onto a lookalike. It also decodes raw YOLO output tensors and prepares re-identification crops, so a detector and an appearance model can feed it directly.

```rust
use ados_protocol::framebus::{BoundingBox, Detection, LockState};
use ados_vision::tracker::{SingleObjectTracker, TrackState, TrackerConfig};

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
- Appearance-first association with a pluggable [`AppearanceModel`](https://docs.rs/ados-vision/latest/ados_vision/tracker/trait.AppearanceModel.html); a colour/gradient histogram ships, a learned embedding plugs in through the same trait.
- YOLOv8/v11 and YOLOv5/v7 head decoding with class-agnostic non-maximum suppression.
- 63 tests, including adversarial suites that try to provoke identity swaps, locks onto noise and distractor capture.
- One dependency: [`ados-protocol`](https://crates.io/crates/ados-protocol) for the shared `Detection` type.

## How it works

The crate has three modules.

### [`tracker`](https://docs.rs/ados-vision/latest/ados_vision/tracker/index.html)

[`SingleObjectTracker`](https://docs.rs/ados-vision/latest/ados_vision/tracker/struct.SingleObjectTracker.html) takes the per-frame detections (the shared `Detection` type, never a parallel one), predicts where the locked object should be next with a constant-velocity Kalman model over `[cx, cy, w, h]` and their rates, associates the single best detection, and stamps a persistent `track_id`, `lock_state` and `assoc_confidence` on it. A brief miss is coasted on the prediction and keeps the id; only a real loss drops the lock, and a re-acquire then mints a fresh id.

The lifecycle is `Idle`, `Tentative` (too few hits to report), `Confirmed`, and `Coasting` (held on the prediction through a miss or an ambiguous frame, reported `Uncertain`). A track can be seeded automatically or by an explicit operator pick with [`designate`](https://docs.rs/ados-vision/latest/ados_vision/tracker/struct.SingleObjectTracker.html#method.designate); `TrackUpdate::operator_designated` tells the two apart, so a consumer can present only operator-picked tracks as locked.

Geometry alone cannot separate two overlapping boxes: a distractor riding the prediction fits the motion gate as well as the true target that maneuvered off it. So [`update_with_appearance`](https://docs.rs/ados-vision/latest/ados_vision/tracker/struct.SingleObjectTracker.html#method.update_with_appearance) takes [`Candidate`](https://docs.rs/ados-vision/latest/ados_vision/tracker/struct.Candidate.html)s that carry an appearance descriptor beside the detection. The tracker captures a template at confirm and updates it slowly, so it cannot drift onto a distractor. The descriptor travels only through `Candidate`, never on the wire detection.

Association is appearance-first. Among candidates that clear the absolute appearance gate, the one that looks most like the template wins; motion fit and detector confidence only break ties within an ambiguity margin of the best. When two admissible candidates fall inside that margin, the tracker holds on the prediction, reports `Uncertain`, lowers its confidence and sets [`needs_redesignation`](https://docs.rs/ados-vision/latest/ados_vision/tracker/struct.SingleObjectTracker.html#method.needs_redesignation). The one case vision cannot solve, a lone appearance-identical impostor while the true target is out of frame, is left to that operator backstop rather than a guess.

All thresholds live in [`TrackerConfig`](https://docs.rs/ados-vision/latest/ados_vision/tracker/struct.TrackerConfig.html).

### [`yolo`](https://docs.rs/ados-vision/latest/ados_vision/yolo/index.html)

[`decode`](https://docs.rs/ados-vision/latest/ados_vision/yolo/fn.decode.html) turns a raw output tensor into detections in source-frame pixels. It supports the transposed `[1, 4+nc, anchors]` YOLOv8/v11 head (no objectness) and the `[1, anchors, 5+nc]` YOLOv5/v7 head, transposes a head delivered the other way round automatically, scales boxes by `frame / input`, applies the confidence gate and runs class-agnostic NMS. [`preprocess_rgb24_nchw`](https://docs.rs/ados-vision/latest/ados_vision/yolo/fn.preprocess_rgb24_nchw.html) does the matching stretch resize into an NCHW float tensor.

### [`reid`](https://docs.rs/ados-vision/latest/ados_vision/reid/index.html)

Helpers for a re-identification model such as OSNet: crop and resize a detection box out of an rgb24 frame, normalize the crop with ImageNet statistics into an NCHW tensor, and L2-normalize the returned embedding so cosine similarity becomes a dot product.

## Part of ADOS

Used in production by the [ADOS Drone Agent](https://github.com/altnautica/ADOSDroneAgent), the onboard software of the ADOS software-defined drone platform.
[All ADOS crates](https://github.com/altnautica/ADOSDroneAgent/tree/main/crates#published-crates) · [Docs](https://docs.altnautica.com) · [Discord](https://discord.gg/uxbvuD4d5q)

### Other ADOS crates

<!-- ados-family:crates -->
**Platform (versioned with the ADOS Drone Agent)**

| Crate | What it does | Docs |
|---|---|---|
| [`ados-protocol`](https://crates.io/crates/ados-protocol) | Wire contracts of the ADOS Drone Agent: IPC framing, plugin RPC envelope, capability tokens and the vision frame bus. | [docs.rs](https://docs.rs/ados-protocol) |
| [`ados-sdk`](https://crates.io/crates/ados-sdk) | Write Rust plugins for the ADOS Drone Agent: IPC client, plugin context, driver traits and lifecycle runner. | [docs.rs](https://docs.rs/ados-sdk) |
| [`ados-gpio`](https://crates.io/crates/ados-gpio) | Drive status buzzers and LEDs on Linux GPIO lines, with bounded software-PWM beep patterns. | [docs.rs](https://docs.rs/ados-gpio) |
| [`ados-hal-probe`](https://crates.io/crates/ados-hal-probe) | Find out what a board's hardware can really do by probing it, plus bundled board profiles. | [docs.rs](https://docs.rs/ados-hal-probe) |

**Standalone**

| Crate | What it does | Docs |
|---|---|---|
| [`ados-sensorhub`](https://crates.io/crates/ados-sensorhub) | Sensor hub wire protocol: COBS framing, CRC-16 and fixed-layout IMU, baro, mag and GNSS packets. no_std. | [docs.rs](https://docs.rs/ados-sensorhub) |
| [`ados-offload`](https://crates.io/crates/ados-offload) | Choose where perception runs (onboard, offloaded or hybrid) and hold the vehicle when results go stale. | [docs.rs](https://docs.rs/ados-offload) |
| [`ados-macpin`](https://crates.io/crates/ados-macpin) | Stable MAC addresses for Linux network adapters that randomize theirs every boot. | [docs.rs](https://docs.rs/ados-macpin) |
| [`ados-lora`](https://crates.io/crates/ados-lora) | LoRa transmit governor with cited regional band plans, duty cycle, dwell time and airtime accounting. | [docs.rs](https://docs.rs/ados-lora) |
| [`ados-crsf-codec`](https://crates.io/crates/ados-crsf-codec) | no_std CRSF / ExpressLRS codec for RC channels, telemetry and parameter frames. | [docs.rs](https://docs.rs/ados-crsf-codec) |
| [`ados-positioning`](https://crates.io/crates/ados-positioning) | Split mixed UBX, NMEA and RTCM3 GNSS streams, configure u-blox receivers and run an RTK base survey-in. | [docs.rs](https://docs.rs/ados-positioning) |
| [`ados-rtcm`](https://crates.io/crates/ados-rtcm) | RTCM3 framing, CRC-24Q and MAVLink GPS_RTCM_DATA fragmentation for RTK corrections. | [docs.rs](https://docs.rs/ados-rtcm) |
| [`ados-clock`](https://crates.io/crates/ados-clock) | Map remote clocks onto the host clock: least-squares and envelope fits, TIMESYNC and a chrony sink. | [docs.rs](https://docs.rs/ados-clock) |
| [`ados-swarm`](https://crates.io/crates/ados-swarm) | Swarm control laws: separation, flocking, formations, CBBA task allocation and a point-mass simulator. | [docs.rs](https://docs.rs/ados-swarm) |
| [`ados-mavlink-tunnel`](https://crates.io/crates/ados-mavlink-tunnel) | Send large messages over MAVLink TUNNEL frames with chunking and bounded reassembly. | [docs.rs](https://docs.rs/ados-mavlink-tunnel) |
| [`ados-display-driver`](https://crates.io/crates/ados-display-driver) | Small-display output on Linux: SSD1306/SH1106 OLED over I2C, pixel packers and a framebuffer writer. | [docs.rs](https://docs.rs/ados-display-driver) |
| [`ados-hmi`](https://crates.io/crates/ados-hmi) | Front-panel input: touchscreen calibration and gestures, button presses and rotary encoders. | [docs.rs](https://docs.rs/ados-hmi) |
<!-- /ados-family:crates -->

### ADOS repositories

<!-- ados-family:repos -->
| Repo | Runs on | What it does | Link |
|---|---|---|---|
| ADOS Mission Control | Browser, desktop (Electron) | Ground control station: FC setup, mission planning, flight, 3D simulation, fleet | [ADOSMissionControl](https://github.com/altnautica/ADOSMissionControl) |
| ADOS Drone Agent | Companion computer on the drone, or a ground node | Onboard agent: MAVLink routing, HD video, long-range radio link, plugins, remote control | [ADOSDroneAgent](https://github.com/altnautica/ADOSDroneAgent) |
| ADOS Android GCS | Android phones and tablets | Native ground control app for agents on the LAN, direct flight controllers and the cloud | [ADOSAndroidGCS](https://github.com/altnautica/ADOSAndroidGCS) |
| ADOS Extensions | Drone Agent and Mission Control | First-party signed plugins, each with an agent half and a GCS half | [ADOSExtensions](https://github.com/altnautica/ADOSExtensions) |
| ADOS MCP | Your machine | Model Context Protocol server that lets AI clients operate ADOS drones under scopes and a safety gate | [ADOS-MCP](https://github.com/altnautica/ADOS-MCP) |
| Documentation | docs.altnautica.com | Public docs for every ADOS product | [Documentation](https://github.com/altnautica/Documentation) |
<!-- /ados-family:repos -->

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
