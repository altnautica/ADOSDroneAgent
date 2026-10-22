# ados-mavlink-tunnel

[![crates.io](https://img.shields.io/crates/v/ados-mavlink-tunnel.svg)](https://crates.io/crates/ados-mavlink-tunnel)
[![docs.rs](https://img.shields.io/docsrs/ados-mavlink-tunnel)](https://docs.rs/ados-mavlink-tunnel)
[![license](https://img.shields.io/crates/l/ados-mavlink-tunnel.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-blue.svg)](https://github.com/altnautica/ADOSDroneAgent/blob/main/crates/Cargo.toml)

**Chunk and reassemble large messages over MAVLink TUNNEL frames with a 12-byte header and bounded reassembly.**

A MAVLink `TUNNEL` frame (message id 385) carries at most 128 bytes of opaque payload and gives no delivery guarantee. This crate splits a larger body into numbered chunks that each fit one frame, and puts them back together on the far side, out of order, with duplicates and losses handled and every buffer bounded. Reach for it when you need to send JSON or binary messages of a few hundred bytes to a few kilobytes over a MAVLink link.

```rust
use std::time::Instant;
use ados_mavlink_tunnel::{Framing, PushOutcome, Reassembler, TUNNEL_PAYLOAD_CAP};

// Pick a magic unique to your application.
let framing = Framing { magic: [0x5A, 0x01] };

// Chunk a 2 KB message; each chunk fits one TUNNEL payload.
let body: Vec<u8> = (0..2048u32).map(|i| i as u8).collect();
let chunks = framing.chunk_message(7, false, false, &body, 64).unwrap();
assert_eq!(chunks.len(), 18);
assert!(chunks.iter().all(|c| c.len() <= TUNNEL_PAYLOAD_CAP));

// Reassemble on the far side, even when chunks arrive in reverse order.
let mut re = Reassembler::new(framing, 64, 8 * 1024);
let mut done = None;
for c in chunks.iter().rev() {
    if let PushOutcome::Complete(m) = re.push(c, Instant::now()) {
        done = Some(m);
    }
}
let msg = done.expect("every chunk delivered");
assert_eq!(msg.request_id, 7);
assert_eq!(msg.body, body);
```

## Highlights

- Zero dependencies; standard library only.
- Fixed 12-byte header, so 116 body bytes ride each 128-byte TUNNEL payload.
- Bounded reassembly: per-message chunk and byte caps, plus a deadline sweep, so a flooding peer cannot grow memory without bound.
- Tolerates out-of-order delivery, duplicate chunks and request-id reuse.
- Non-conforming payloads are ignored, never errors, so other traffic can share the same `payload_type`.

## How it works

The crate is pure binary framing plus a reassembler. It does not build the outer MAVLink frame or move bytes over a link: hand each chunk to your MAVLink library as the `TUNNEL` payload, pick a private `payload_type` (above 32767), and feed received payloads to [`Reassembler::push`](https://docs.rs/ados-mavlink-tunnel/latest/ados_mavlink_tunnel/struct.Reassembler.html#method.push).

Wire layout of one chunk payload:

```text
  byte 0..2   magic       Framing::magic, distinguishes your chunks from other
                          traffic sharing the payload_type
  byte 2      version     = 0x01 (L3_VERSION)
  byte 3      flags       bit0 = is_response, bit1 = is_error, rest reserved 0
  byte 4..8   request_id  u32 little-endian, correlation id
  byte 8..10  seq         u16 little-endian, 0-based chunk index
  byte 10..12 total       u16 little-endian, total chunk count
  byte 12..   data        up to MAX_CHUNK_DATA (116) bytes of the body
```

A whole body is the concatenation of the chunk data slices in `seq` order. An empty body travels as one zero-length chunk.

- [`Framing`](https://docs.rs/ados-mavlink-tunnel/latest/ados_mavlink_tunnel/struct.Framing.html) holds the magic and provides `build_chunk`, `parse_chunk` and `chunk_message`.
- [`Reassembler`](https://docs.rs/ados-mavlink-tunnel/latest/ados_mavlink_tunnel/struct.Reassembler.html) keeps in-flight messages keyed by `request_id`. A chunk whose shape (total, direction, error flag) disagrees with an in-flight message under the same id starts a fresh message. `sweep` drops messages older than a deadline and returns how many it dropped.
- [`PushOutcome`](https://docs.rs/ados-mavlink-tunnel/latest/ados_mavlink_tunnel/enum.PushOutcome.html) reports `Ignored`, `Incomplete`, `Complete` or `Rejected` with a reason.

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
