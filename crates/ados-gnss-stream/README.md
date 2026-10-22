# ados-gnss-stream

[![crates.io](https://img.shields.io/crates/v/ados-gnss-stream.svg)](https://crates.io/crates/ados-gnss-stream)
[![docs.rs](https://img.shields.io/docsrs/ados-gnss-stream)](https://docs.rs/ados-gnss-stream)
[![license](https://img.shields.io/crates/l/ados-gnss-stream.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-blue.svg)](https://github.com/altnautica/ADOSDroneAgent/blob/main/crates/Cargo.toml)

**Split a mixed UBX, NMEA and RTCM3 byte stream with resync, build u-blox VALSET commands and run an RTK base survey-in state machine.**

A GNSS receiver interleaves several protocols on one serial or USB port, and a read can end in the middle of any of them. This crate buffers the bytes, hands back whole frames whose checksums hold, and skips garbage one byte at a time until it finds the next valid frame. On top of the splitter it decodes the common u-blox navigation messages and NMEA GGA/RMC, builds `CFG-VALSET` configuration commands, and drives an RTK base receiver through survey-in or a fixed position.

```rust
use ados_gnss_stream::stream::{crc24q, ubx_frame, Frame, Splitter};

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

- [`stream`](https://docs.rs/ados-gnss-stream/latest/ados_gnss_stream/stream/) splits one receiver byte stream into [`Frame`](https://docs.rs/ados-gnss-stream/latest/ados_gnss_stream/stream/enum.Frame.html)s. Receivers interleave protocols on one port, and a read can end anywhere, so [`Splitter`](https://docs.rs/ados-gnss-stream/latest/ados_gnss_stream/stream/struct.Splitter.html) buffers across pushes. A frame is accepted only when its checksum holds; on a mismatch, or on bytes that start no known frame, one byte is dropped and the scan resumes. The dropped byte count is available as a garbage counter. It also exposes `ubx_frame`, `ubx_checksum`, `crc24q` and `rtcm_msg_type` for building and inspecting frames.
- [`ubx`](https://docs.rs/ados-gnss-stream/latest/ados_gnss_stream/ubx/) decodes NAV-PVT, NAV-DOP, NAV-SVIN, NAV-RELPOSNED, TIM-TP, MON-VER and ACK payloads. Offsets follow the u-blox F9 interface description; every decoder checks the payload length first and returns `None` for a short or foreign payload.
- [`nmea`](https://docs.rs/ados-gnss-stream/latest/ados_gnss_stream/nmea/) decodes GGA and RMC, the fallback when a receiver sends no UBX.
- [`receiver`](https://docs.rs/ados-gnss-stream/latest/ados_gnss_stream/receiver/) keeps per-receiver navigation state fed by decoded frames and reports a solution (fix type, RTK state, position, accuracy) that goes stale after a fixed timeout.
- [`valset`](https://docs.rs/ados-gnss-stream/latest/ados_gnss_stream/valset/) builds UBX-CFG-VALSET commands. A key id carries its value size in bits 28..30 (1 = one bit stored in a byte, 2 = one byte, 3 = two, 4 = four, 5 = eight), and each item writes exactly that many little-endian bytes. Helpers enable the navigation outputs per port, start a survey-in, set a fixed base position and enable the RTCM3 base messages 1005, 1074, 1084, 1094, 1124 and 1230.
- [`base`](https://docs.rs/ados-gnss-stream/latest/ados_gnss_stream/base/) is the RTK base state machine: `off`, then `surveying`, then `fixed` or `failed`. The machine returns the bytes to write to the receiver, and receiver reports (NAV-SVIN, ACK, MON-VER) advance it. Nothing is reported as progress until the receiver itself reports it, and a command the receiver does not confirm within a timeout fails the machine.

## Cargo features

|Feature|Default|What it enables|
|---|---|---|
|`fixtures`|no|The `fixtures` module: byte-exact receiver frames with correct checksums, for tests in downstream crates.|

## Part of ADOS

Used in production by the [ADOS Drone Agent](https://github.com/altnautica/ADOSDroneAgent), the onboard software of the ADOS software-defined drone platform.
[All ADOS crates](https://github.com/altnautica/ADOSDroneAgent/tree/main/crates#published-crates) · [Docs](https://docs.altnautica.com) · [Discord](https://discord.gg/uxbvuD4d5q)

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
