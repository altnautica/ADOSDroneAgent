# ados-hmi

[![crates.io](https://img.shields.io/crates/v/ados-hmi.svg)](https://crates.io/crates/ados-hmi)
[![docs.rs](https://img.shields.io/docsrs/ados-hmi)](https://docs.rs/ados-hmi)
[![license](https://img.shields.io/crates/l/ados-hmi.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-blue.svg)](https://github.com/altnautica/ADOSDroneAgent/blob/main/crates/Cargo.toml)

**Input state machines for hardware front panels: touchscreen affine calibration and gestures, button press classification, quadrature encoder decoding and a pilot-in-command arbiter.**

A small device with a few buttons, a rotary knob and a resistive touchscreen needs the same logic every time: debounce the buttons, tell a short press from a long one, turn noisy encoder edges into clean detents, map raw touch counts onto display pixels and recognise taps and swipes. This crate holds those pieces as pure state machines that take timestamped events, so you wire them to GPIO or evdev yourself and test them without hardware.

```rust
use std::collections::HashMap;
use ados_hmi::encoder::{Channel, QuadratureDecoder, Rotation};
use ados_hmi::press::{Edge, PressClassifier, PressKind};

// Four buttons on GPIO 5, 6, 13 and 19 are labelled B1..B4.
let mapping: HashMap<String, String> = [("B1_short", "next"), ("B1_long", "menu")]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
let mut buttons = PressClassifier::from_mapping(mapping, [5, 6, 13, 19]);

// Press at t=0 ms, a contact bounce 8 ms later, release after 2.5 s: one long press.
assert!(buttons.on_edge(5, Edge::Falling, 0).is_none());
assert!(buttons.on_edge(5, Edge::Falling, 8).is_none());
let event = buttons.on_edge(5, Edge::Rising, 2_500).unwrap();
assert_eq!(event.kind, PressKind::Long);
assert_eq!(event.action.as_deref(), Some("menu"));

// One clockwise detent of a KY-040 style encoder: A leads B.
let mut knob = QuadratureDecoder::new(true, true);
let mut detents = Vec::new();
for (line, high) in [(Channel::A, false), (Channel::B, false), (Channel::A, true), (Channel::B, true)] {
    detents.extend(knob.on_edge(line, high));
}
assert_eq!(detents, [Rotation::Cw]);
```

## Highlights

- Every state machine takes caller-supplied timestamps or an injectable clock, so debounce windows, hold thresholds, token TTLs and watchdogs are tested without sleeping.
- Quadrature decoding counts quarter steps, so contact bounce cancels out and a missed edge is rejected instead of producing a wrong detent.
- Touch calibration fits a 2x3 affine matrix by least squares from any number of points and reports the RMS residual.
- Action mappings sit behind a shared `Arc<RwLock<_>>`, so a remap takes effect on the next release without rebuilding the classifier.
- Event buses are bounded `tokio::sync::broadcast` channels: a slow subscriber lags instead of stalling the publisher.
- Builds on any host; nothing touches GPIO, evdev or a display directly.

## How it works

- [`press`](https://docs.rs/ados-hmi/latest/ados_hmi/press/): active-low buttons. A falling edge records the press unless it lands inside the 20 ms debounce window of the previous press on that pin. The rising edge classifies the hold: under 2 s is `short`, 2 s to 6 s is `long`, 6 s or more is dropped as a cancel. The action is looked up as `<label>_<kind>`, where the label is `B1`..`B4` by role order, `BX<pin>` for other pins, or `ENC_SW` for an encoder push switch.
- [`encoder`](https://docs.rs/ados-hmi/latest/ados_hmi/encoder/): the 2-bit Gray code `A << 1 | B` walks four states per detent. Each valid single-line transition adds or removes a quarter step; a detent is reported only on the return to rest with a net count of four in one direction. A transition where both lines changed is counted as rejected. Detents can be fed into `PressClassifier::on_rotation` and resolve as `ENC_CW_short` / `ENC_CCW_short`.
- [`affine`](https://docs.rs/ados-hmi/latest/ados_hmi/affine/): maps raw 12-bit touch ADC counts to display pixels. Covers the four rotation identities, the normal-equations least-squares fit, a Gaussian-elimination solver with partial pivot and singular guard, round-half-to-even application, and atomic JSON persistence that rejects a stored calibration whose panel size or rotation no longer matches.
- [`touch`](https://docs.rs/ados-hmi/latest/ados_hmi/touch/): a per-stroke state machine with a move filter, classifying each stroke as tap, long press, swipe or drag by duration and displacement, with direction taken from the larger axis.
- [`pic`](https://docs.rs/ados-hmi/latest/ados_hmi/pic/): a pilot-in-command arbiter. Exactly one client holds control; a takeover needs a confirm token or a force flag, a missed heartbeat auto-releases, and a connected gamepad can auto-claim an unheld seat. Each transition is published on [`eventbus`](https://docs.rs/ados-hmi/latest/ados_hmi/eventbus/).

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
