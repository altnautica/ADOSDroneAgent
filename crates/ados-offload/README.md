# ados-offload

[![crates.io](https://img.shields.io/crates/v/ados-offload.svg)](https://crates.io/crates/ados-offload)
[![docs.rs](https://img.shields.io/docsrs/ados-offload)](https://docs.rs/ados-offload)
[![license](https://img.shields.io/crates/l/ados-offload.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-blue.svg)](https://github.com/altnautica/ADOSDroneAgent/blob/main/crates/Cargo.toml)

**Decide where drone perception runs (onboard, offloaded or hybrid) and gate stale or link-lost results so the vehicle holds instead of extrapolating.**

A drone without an NPU can still fly autonomous behaviours if a nearby compute node runs detection, tracking and SLAM for it. Remote results arrive late and sometimes not at all, and a control loop that acts on a frozen result follows something that is no longer there. This crate makes the two decisions that keep that safe: where perception runs, and whether the latest result is fresh enough to act on. It does no I/O, so it fits any transport.

```rust
use ados_offload::{
    pick_tier, GateState, LockState, OffloadMode, OffloadSession, PerceptionTier, TierInputs,
};

// No accelerator, but a compute node is paired on a fast enough link.
let inputs = TierInputs::for_drone(false, false, true, true);
assert_eq!(pick_tier(&inputs), Some(PerceptionTier::Offload));

// Follow a target with a 300 ms freshness budget on detections.
let mut session = OffloadSession::new(OffloadMode::VisionOnly, 300, 300);
session.designate();
session.on_detection(1_000);
assert!(session.tick(1_100).commanding);

// The node stalls: 500 ms later the result is stale, so the drone holds.
let status = session.tick(1_600);
assert_eq!(status.target, GateState::Stale);
assert!(!status.commanding);
assert_eq!(status.lock, LockState::Lost);

// A fresh result alone does not resume; only an explicit re-designation does.
session.on_detection(1_700);
assert!(!session.tick(1_750).commanding);
session.designate();
assert!(session.tick(1_760).commanding);
```

## Highlights

- Pure decision logic: no I/O, no threads, no clock reads. Every time is passed in by the caller.
- Fail-safe by construction: stale or link-lost means stop and hold, never extrapolate, and a lost lock never re-acquires on its own.
- Freshness is measured on the drone's own monotonic clock, so a remote node with a skewed clock cannot make a frozen stream look fresh.
- A negative freshness budget is clamped to zero rather than flipping the fail-safe direction.
- One dependency (`serde`, for the state enums); 30 unit tests.

## How it works

The rule the crate is built around: the fast control loop and the control-loop pose stay on the drone; the heavy perception and the drift-corrected map pose may go remote.

- [`pick_tier`](https://docs.rs/ados-offload/latest/ados_offload/fn.pick_tier.html) chooses a [`PerceptionTier`](https://docs.rs/ados-offload/latest/ados_offload/enum.PerceptionTier.html) from [`TierInputs`](https://docs.rs/ados-offload/latest/ados_offload/struct.TierInputs.html):
  - a local accelerator that fits the models, or a CPU strong enough for the detector, gives **Local**;
  - otherwise a paired node on an acceptable link gives **Hybrid** when the board can run a light local path, else **Offload**;
  - otherwise a board with a light local path gives degraded **Local**;
  - otherwise `None`: no heavy perception, the drone flies on bare odometry.
- [`FreshnessGate`](https://docs.rs/ados-offload/latest/ados_offload/struct.FreshnessGate.html) tracks one return stream. It is `Fresh` when a result arrived within the budget and the link is up, `Stale` when the last result is too old, and `Lost` before any result or while the link is down. Only `Fresh` is usable.
- [`LockGate`](https://docs.rs/ados-offload/latest/ados_offload/struct.LockGate.html) is the lock state machine. A lock that has had a usable reading drops to `Lost` the moment the stream stops being usable. A lock still acquiring (never usable yet) holds in `Locked`. The only way out of `Lost` is an explicit `lock()`.
- [`OffloadSession`](https://docs.rs/ados-offload/latest/ados_offload/struct.OffloadSession.html) composes a target gate, a pose gate and a lock gate per [`OffloadMode`](https://docs.rs/ados-offload/latest/ados_offload/enum.OffloadMode.html): `VisionOnly` needs fresh detections, `SlamOnly` fresh poses, `Full` both. Each `tick` returns a [`SessionStatus`](https://docs.rs/ados-offload/latest/ados_offload/struct.SessionStatus.html) whose `commanding` flag is true only when locked and every required stream is fresh.

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
| [`ados-vision`](https://crates.io/crates/ados-vision) | Single-object visual tracker: Kalman motion model, appearance re-identification and YOLOv8 head decoding. | [docs.rs](https://docs.rs/ados-vision) |
| [`ados-gpio`](https://crates.io/crates/ados-gpio) | Drive status buzzers and LEDs on Linux GPIO lines, with bounded software-PWM beep patterns. | [docs.rs](https://docs.rs/ados-gpio) |
| [`ados-hal-probe`](https://crates.io/crates/ados-hal-probe) | Find out what a board's hardware can really do by probing it, plus bundled board profiles. | [docs.rs](https://docs.rs/ados-hal-probe) |

**Standalone**

| Crate | What it does | Docs |
|---|---|---|
| [`ados-sensorhub`](https://crates.io/crates/ados-sensorhub) | Sensor hub wire protocol: COBS framing, CRC-16 and fixed-layout IMU, baro, mag and GNSS packets. no_std. | [docs.rs](https://docs.rs/ados-sensorhub) |
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
