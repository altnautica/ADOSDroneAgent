# ados-positioning

[![crates.io](https://img.shields.io/crates/v/ados-positioning.svg)](https://crates.io/crates/ados-positioning)
[![docs.rs](https://img.shields.io/docsrs/ados-positioning)](https://docs.rs/ados-positioning)
[![license](https://img.shields.io/crates/l/ados-positioning.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-blue.svg)](https://github.com/altnautica/ADOSDroneAgent/blob/main/crates/Cargo.toml)

**Split a mixed UBX, NMEA and RTCM3 byte stream with resync, build u-blox VALSET commands and run an RTK base survey-in state machine.**

A GNSS receiver interleaves several protocols on one serial or USB port, and a read can end in the middle of any of them. This crate buffers the bytes, hands back whole frames whose checksums hold, and skips garbage one byte at a time until it finds the next valid frame. On top of the splitter it decodes the common u-blox navigation messages and NMEA GGA/RMC, builds `CFG-VALSET` configuration commands, and drives an RTK base receiver through survey-in or a fixed position.

```rust
use ados_positioning::stream::{crc24q, ubx_frame, Frame, Splitter};

// A UBX NAV-DOP message with an 18-byte payload.
let ubx = ubx_frame(0x01, 0x04, &[0u8; 18]);

// An NMEA sentence: `$body*hh` where hh is the XOR of the body bytes.
let body = "GNGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,";
let ck = body.bytes().fold(0u8, |a, c| a ^ c);
let nmea = format!("${body}*{ck:02X}\r\n");

// An RTCM3 frame: preamble, 10-bit length, payload, CRC-24Q. The payload
// starts with the 12-bit message number (1005 = 0x3ED).
let mut rtcm = vec![0xD3, 0x00, 0x02, 0x3E, 0xD0];
let crc = crc24q(&rtcm);
rtcm.extend_from_slice(&[(crc >> 16) as u8, (crc >> 8) as u8, crc as u8]);

// Interleave them with line noise, then feed the stream in small reads.
let mut bytes = vec![0x00, 0xFF, b'x'];
bytes.extend_from_slice(&ubx);
bytes.extend_from_slice(nmea.as_bytes());
bytes.extend_from_slice(&[0x13, 0x37]);
bytes.extend_from_slice(&rtcm);

let mut splitter = Splitter::new();
let frames: Vec<Frame> = bytes.chunks(5).flat_map(|c| splitter.push(c)).collect();

assert!(matches!(frames[0], Frame::Ubx { class: 0x01, id: 0x04, .. }));
assert!(matches!(&frames[1], Frame::Nmea(s) if s.starts_with("$GNGGA")));
assert_eq!(frames[2], Frame::Rtcm(rtcm));
assert!(splitter.garbage() > 0);
```

## Highlights

- One splitter for UBX, NMEA 0183 and RTCM3: every frame is accepted only when its UBX Fletcher checksum, NMEA XOR checksum or RTCM3 CRC-24Q holds.
- Bounded memory: the buffer is capped at 64 KiB, UBX payloads at 8 KiB and NMEA sentences at 120 bytes, so a stream that never forms a frame cannot grow without limit.
- Pure state machines: the RTK base machine returns the bytes to write and takes the receiver's reports as input, with no I/O of its own.
- Unit tests cover chunked input at every split size, resync after corrupt frames, the VALSET key encoding and every base transition.
- Two dependencies: `serde` for the serializable state types and `tracing` for the base failure event.

## How it works

The crate has six modules.

- [`stream`](https://docs.rs/ados-positioning/latest/ados_positioning/stream/) splits one receiver byte stream into [`Frame`](https://docs.rs/ados-positioning/latest/ados_positioning/stream/enum.Frame.html)s. Receivers interleave protocols on one port, and a read can end anywhere, so [`Splitter`](https://docs.rs/ados-positioning/latest/ados_positioning/stream/struct.Splitter.html) buffers across pushes. A frame is accepted only when its checksum holds; on a mismatch, or on bytes that start no known frame, one byte is dropped and the scan resumes. The dropped byte count is available as a garbage counter. It also exposes `ubx_frame`, `ubx_checksum`, `crc24q` and `rtcm_msg_type` for building and inspecting frames.
- [`ubx`](https://docs.rs/ados-positioning/latest/ados_positioning/ubx/) decodes NAV-PVT, NAV-DOP, NAV-SVIN, NAV-RELPOSNED, TIM-TP, MON-VER and ACK payloads. Offsets follow the u-blox F9 interface description; every decoder checks the payload length first and returns `None` for a short or foreign payload.
- [`nmea`](https://docs.rs/ados-positioning/latest/ados_positioning/nmea/) decodes GGA and RMC, the fallback when a receiver sends no UBX.
- [`receiver`](https://docs.rs/ados-positioning/latest/ados_positioning/receiver/) keeps per-receiver navigation state fed by decoded frames and reports a solution (fix type, RTK state, position, accuracy) that goes stale after a fixed timeout.
- [`valset`](https://docs.rs/ados-positioning/latest/ados_positioning/valset/) builds UBX-CFG-VALSET commands. A key id carries its value size in bits 28..30 (1 = one bit stored in a byte, 2 = one byte, 3 = two, 4 = four, 5 = eight), and each item writes exactly that many little-endian bytes. Helpers enable the navigation outputs per port, start a survey-in, set a fixed base position and enable the RTCM3 base messages 1005, 1074, 1084, 1094, 1124 and 1230.
- [`base`](https://docs.rs/ados-positioning/latest/ados_positioning/base/) is the RTK base state machine: `off`, then `surveying`, then `fixed` or `failed`. The machine returns the bytes to write to the receiver, and receiver reports (NAV-SVIN, ACK, MON-VER) advance it. Nothing is reported as progress until the receiver itself reports it, and a command the receiver does not confirm within a timeout fails the machine.

## Cargo features

|Feature|Default|What it enables|
|---|---|---|
|`fixtures`|no|The `fixtures` module: byte-exact receiver frames with correct checksums, for tests in downstream crates.|

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
