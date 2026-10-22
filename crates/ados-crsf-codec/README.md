# ados-crsf-codec

[![crates.io](https://img.shields.io/crates/v/ados-crsf-codec.svg)](https://crates.io/crates/ados-crsf-codec)
[![docs.rs](https://img.shields.io/docsrs/ados-crsf-codec)](https://docs.rs/ados-crsf-codec)
[![license](https://img.shields.io/crates/l/ados-crsf-codec.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-blue.svg)](https://github.com/altnautica/ADOSDroneAgent/blob/main/crates/Cargo.toml)

**no_std CRSF (Crossfire / ExpressLRS) codec: frames, packed RC channels, telemetry and parameter frames, axis scaling and link liveness.**

CRSF is the serial protocol spoken by Crossfire and ExpressLRS radio modules,
receivers and flight controllers. This crate encodes and decodes it: build an
RC channels frame for a transmitter module, hunt frames out of a noisy byte
stream, decode the telemetry a module sends back, and decide from link
statistics whether the radio link is actually up. It is pure data handling
with no I/O, so it runs the same on a microcontroller and on a Linux host.

```rust
use ados_crsf_codec::channels::{build_rc_frame, unpack_channels, CHANNEL_COUNT, CHANNEL_MID, PACKED_SIZE};
use ados_crsf_codec::frame::{Parser, ADDR_FLIGHT_CONTROLLER, TYPE_RC_CHANNELS_PACKED};

// 16 channels at center, with throttle low and one switch high.
let mut channels = [CHANNEL_MID; CHANNEL_COUNT];
channels[2] = 172;
channels[4] = 1811;

// One complete wire frame: sync, length, type, 22-byte payload, CRC-8.
let wire = build_rc_frame(&channels).unwrap();
assert_eq!(wire.len(), 26);

// Feed the bytes to the stream parser, split across two reads and
// preceded by line noise.
let mut parser = Parser::new();
assert!(parser.push(&[0x00, 0x42]).is_empty());
let mut frames = parser.push(&wire[..10]);
frames.extend(parser.push(&wire[10..]));
assert_eq!(frames.len(), 1);

let frame = &frames[0];
assert_eq!(frame.sync, ADDR_FLIGHT_CONTROLLER);
assert_eq!(frame.frame_type, TYPE_RC_CHANNELS_PACKED);
let payload: &[u8; PACKED_SIZE] = frame.payload.as_slice().try_into().unwrap();
assert_eq!(unpack_channels(payload), channels);
```

## Highlights

- `#![no_std]` with `alloc`; zero dependencies. Builds for bare-metal targets such as `thumbv7em-none-eabihf`.
- The stream parser never panics on garbage: it resyncs byte by byte and counts the bytes it skipped.
- Codecs round-trip: every typed frame has an encoder and a decoder, and 61 unit tests check wire layouts and round trips.
- Payload sizes are validated against the frame type before decoding, so a short or long payload is an error, not a panic.
- Link liveness is a pure function: no clocks, no threads, just durations in and a state out.

## How it works

The crate has seven modules.

- [`frame`](https://docs.rs/ados-crsf-codec/latest/ados_crsf_codec/frame/) is the byte framing. Every frame is
  `[sync] [len] [type] [payload...] [crc8]`. `len` counts the bytes after it
  (type, payload and CRC, so 2..=62). The CRC-8 uses the DVB-S2 polynomial
  `0xD5` (init 0, no reflection, no xor-out) and covers the type and payload
  only. The sync byte is a device address: `0xC8` (flight controller), `0xEE`
  (transmitter module) or `0xEA` (radio handset); anything else is rejected.
  `Parser` buffers partial input and returns every complete, CRC-valid frame.
- [`channels`](https://docs.rs/ados-crsf-codec/latest/ados_crsf_codec/channels/) packs 16 channels of 11 bits each,
  LSB first, into the 22-byte `RC_CHANNELS_PACKED` (0x16) payload. `172` is
  full low, `992` center and `1811` full high. The codec round-trips any
  11-bit value; clamping to the usable range is the caller's choice.
- [`bank`](https://docs.rs/ados-crsf-codec/latest/ados_crsf_codec/bank/) is a validated set of 16 channel values: writes
  outside 172..=1811 are rejected, and a bulk write is all or nothing.
- [`scale`](https://docs.rs/ados-crsf-codec/latest/ados_crsf_codec/scale/) maps a signed 16-bit axis onto the channel range
  (with an exact 992 center) and an N-position switch onto evenly spaced
  channel values.
- [`telemetry`](https://docs.rs/ados-crsf-codec/latest/ados_crsf_codec/telemetry/) decodes and encodes link statistics
  (0x14), GPS (0x02), battery (0x08), attitude (0x1E) and flight mode (0x21).
  Multi-byte fields are big-endian.
- [`params`](https://docs.rs/ados-crsf-codec/latest/ados_crsf_codec/params/) carries the parameter protocol: read (0x2E),
  write (0x2F) and settings entry (0x2D) frames.
- [`link`](https://docs.rs/ados-crsf-codec/latest/ados_crsf_codec/link/) derives a link state from what was observed.
  Bytes accepted by a serial driver prove nothing about the radio, so the
  verdict keys on fresh link-statistics telemetry and its uplink link
  quality. Transmitting with no fresh statistics past a grace window, or with
  uplink quality at zero, reads `RfUnverified`; only `LinkOk` and `Degraded`
  count as flyable.

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
