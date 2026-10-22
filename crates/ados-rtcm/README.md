# ados-rtcm

[![crates.io](https://img.shields.io/crates/v/ados-rtcm.svg)](https://crates.io/crates/ados-rtcm)
[![docs.rs](https://img.shields.io/docsrs/ados-rtcm)](https://docs.rs/ados-rtcm)
[![license](https://img.shields.io/crates/l/ados-rtcm.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-blue.svg)](https://github.com/altnautica/ADOSDroneAgent/blob/main/crates/Cargo.toml)

**RTCM3 framing, CRC-24Q and MAVLink GPS_RTCM_DATA fragmentation for injecting RTK corrections into a flight controller.**

An RTK rover needs a steady stream of RTCM3 correction messages from a base station, and a flight controller running ArduPilot or PX4 accepts them over MAVLink as `GPS_RTCM_DATA` messages of at most 180 bytes each. This crate finds whole, CRC-checked RTCM3 frames in a byte stream that arrives in arbitrary pieces, and splits each frame into the fragments and flag bits the firmware reassembles. Reach for it when you forward corrections from a base receiver, an NTRIP client or a file to a vehicle.

```rust
use ados_rtcm::{crc24q, encode_rtcm3, split_rtcm3, Fragmenter, Rtcm3Splitter};

// Frame a 394-byte RTCM3 payload: preamble, 10-bit length, payload, CRC-24Q.
let payload: Vec<u8> = (0..394).map(|i| i as u8).collect();
let frame = encode_rtcm3(&payload);
assert_eq!(frame.len(), 400);

// The trailing three bytes are the CRC-24Q of everything before them.
let (body, crc) = frame.split_at(frame.len() - 3);
assert_eq!(crc24q(body).to_be_bytes()[1..], *crc);

// Junk before the frame is skipped; a stream cut in two still yields one frame.
let mut stream = vec![0x00, 0x11];
stream.extend_from_slice(&frame);
assert_eq!(split_rtcm3(&stream), vec![&frame[..]]);
let mut splitter = Rtcm3Splitter::default();
assert!(splitter.push(&stream[..100]).is_empty());
let frames = splitter.push(&stream[100..]);

// Fragment into GPS_RTCM_DATA payloads: 180 + 180 + 40 bytes.
let mut fragmenter = Fragmenter::default();
let fragments = fragmenter.fragment(&frames[0]).unwrap();
let lens: Vec<u8> = fragments.iter().map(|f| f.len).collect();
assert_eq!(lens, [180, 180, 40]);
assert_eq!(fragments[1].flags, 1 | (1 << 1)); // fragmented, id 1, sequence 0
```

## Highlights

- Zero dependencies, standard library only.
- Stream splitter with resync: junk bytes and frames with a bad CRC are skipped, and the carried partial frame is bounded by one maximum RTCM3 frame (1029 bytes).
- Fragment flags and end-of-message rules match the `GPS_RTCM_DATA` reassembly in flight-controller firmware, including the zero-length closing fragment.
- Unit tests cover fragment counts at every boundary, sequence id wraparound, CRC check values and splitting at every cut point of a frame.

## How it works

**RTCM3 frames.** A frame is the preamble `0xD3`, 6 reserved bits, a 10-bit payload length, the payload (at most 1023 bytes) and a 24-bit CRC-24Q (polynomial `0x1864CFB`, initial value 0) over everything before it. [`encode_rtcm3`](https://docs.rs/ados-rtcm/latest/ados_rtcm/fn.encode_rtcm3.html) builds one and [`crc24q`](https://docs.rs/ados-rtcm/latest/ados_rtcm/fn.crc24q.html) computes the check.

**Splitting.** [`split_rtcm3`](https://docs.rs/ados-rtcm/latest/ados_rtcm/fn.split_rtcm3.html) returns every whole CRC-valid frame in one buffer and drops an incomplete tail. [`Rtcm3Splitter`](https://docs.rs/ados-rtcm/latest/ados_rtcm/struct.Rtcm3Splitter.html) does the same for a stream delivered in pieces, keeping the partial frame for the next `push`. A preamble whose frame fails its CRC is skipped by one byte so the scan resyncs on the next preamble.

**Fragmentation.** `GPS_RTCM_DATA` carries at most 180 bytes ([`FRAGMENT_LEN`](https://docs.rs/ados-rtcm/latest/ados_rtcm/constant.FRAGMENT_LEN.html)). A message of up to 180 bytes goes out whole with `flags = 0`. A longer one is split into 180-byte fragments with `flags = 1 | fragment_id << 1 | (sequence_id & 0x1f) << 3`, and [`Fragmenter`](https://docs.rs/ados-rtcm/latest/ados_rtcm/struct.Fragmenter.html) advances its 5-bit sequence id once per fragmented message, so keep one `Fragmenter` per output link. The receiver treats a fragmented message as complete at the first fragment shorter than 180 bytes, or once all four fragments arrived. A message that is an exact multiple of 180 bytes below 720 therefore ends with a zero-length fragment, and 720 bytes ([`MAX_MESSAGE_LEN`](https://docs.rs/ados-rtcm/latest/ados_rtcm/constant.MAX_MESSAGE_LEN.html)) is four full fragments. Longer messages return [`TooLong`](https://docs.rs/ados-rtcm/latest/ados_rtcm/struct.TooLong.html). [`message_fragments`](https://docs.rs/ados-rtcm/latest/ados_rtcm/fn.message_fragments.html) and [`fragment_count`](https://docs.rs/ados-rtcm/latest/ados_rtcm/fn.fragment_count.html) count the messages ahead of sending.

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
