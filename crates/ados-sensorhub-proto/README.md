# ados-sensorhub-proto

[![crates.io](https://img.shields.io/crates/v/ados-sensorhub-proto.svg)](https://crates.io/crates/ados-sensorhub-proto)
[![docs.rs](https://img.shields.io/docsrs/ados-sensorhub-proto)](https://docs.rs/ados-sensorhub-proto)
[![license](https://img.shields.io/crates/l/ados-sensorhub-proto.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-blue.svg)](https://github.com/altnautica/ADOSDroneAgent/blob/main/crates/Cargo.toml)

**Sensor hub wire protocol: COBS framing, CRC-16/CCITT-FALSE and fixed-layout IMU, barometer, magnetometer, trigger, PPS and GNSS packets. no_std, no allocation.**

A sensor hub is a small microcontroller that timestamps IMU samples, camera triggers and PPS edges on one clock and streams them to a companion computer over USB or a UART. This crate is the wire format both ends share: the same code encodes on the microcontroller and decodes on the host. Reach for it when you build either side of such a link and want byte-exact packets, framing that resynchronises after line noise, and no heap.

```rust
use ados_sensorhub_proto::{
    decode_frame, encode_frame, Deframer, ImuBatch, ImuSample, ImuSamples, Packet, Push,
    MAX_FRAME, MAX_RAW,
};

// Encode two IMU samples taken 1 ms apart as one wire frame.
let samples = [
    ImuSample { gyro: [1, -2, 3], accel: [0, 0, 2048], temp: 3150 },
    ImuSample { gyro: [2, -1, 4], accel: [1, 0, 2049], temp: 3151 },
];
let pkt = Packet::ImuBatch(ImuBatch {
    t0_ns: 5_000_000,
    dt_ns: 1_000_000,
    imu_id: 0,
    flags: 0,
    seq0: 100,
    samples: ImuSamples::Typed(&samples),
});
let mut scratch = [0u8; MAX_RAW];
let mut wire = [0u8; MAX_FRAME];
let n = encode_frame(&pkt, &mut scratch, &mut wire).unwrap();

// The receiver splits the byte stream on 0x00 and decodes each block.
let mut deframer = Deframer::<MAX_FRAME>::new();
let mut buf = [0u8; MAX_RAW];
let mut decoded = 0;
for &b in &wire[..n] {
    if let Push::Block(block) = deframer.push(b) {
        let Packet::ImuBatch(batch) = decode_frame(block, &mut buf).unwrap() else {
            panic!("expected an IMU batch");
        };
        assert_eq!(batch.samples.get(1), Some(samples[1]));
        assert_eq!(batch.sample_t_ns(1), 6_000_000);
        decoded += 1;
    }
}
assert_eq!(decoded, 1);
```

## Highlights

- `#![no_std]`, no allocation: every buffer is caller-owned, sized by the `MAX_RAW` and `MAX_FRAME` constants.
- Zero required dependencies; `serde` derives on the owned packet structs are opt-in.
- `#![forbid(unsafe_code)]`.
- Decoding borrows variable-length payloads (IMU samples, GNSS and RTCM bytes) straight from the decode buffer, with no copy.
- Unknown packet types decode to `Packet::Unknown` instead of failing, so a newer hub does not break an older host.
- Round-trip, framing and malformed-input tests, including proptest round trips.

## How it works

Every packet travels as one frame:

```text
wire  := COBS(raw) 0x00
raw   := type:u8  len:u16le  payload[len]  crc:u16le
crc   := CRC-16/CCITT-FALSE over type, len and payload
```

COBS (Consistent Overhead Byte Stuffing) removes every `0x00` from the raw frame, so `0x00` can only appear as the delimiter. A receiver that starts mid-stream, or loses bytes, recovers at the next delimiter. Payloads are at most [`MAX_PAYLOAD`](https://docs.rs/ados-sensorhub-proto/latest/ados_sensorhub_proto/constant.MAX_PAYLOAD.html) (1024) bytes; multi-byte fields are little-endian, reserved fields are written as zero and ignored on receive, and every `t_ns` is hub time.

- [`encode_frame`](https://docs.rs/ados-sensorhub-proto/latest/ados_sensorhub_proto/fn.encode_frame.html) writes one delimited wire frame; [`encode_raw`](https://docs.rs/ados-sensorhub-proto/latest/ados_sensorhub_proto/fn.encode_raw.html) writes only the raw frame.
- [`Deframer`](https://docs.rs/ados-sensorhub-proto/latest/ados_sensorhub_proto/struct.Deframer.html) splits a byte stream on `0x00` into COBS blocks, one byte at a time. A block longer than its buffer is dropped up to the next delimiter and reported as `Push::Overflow`.
- [`decode_frame`](https://docs.rs/ados-sensorhub-proto/latest/ados_sensorhub_proto/fn.decode_frame.html) unstuffs one block and parses it; [`decode_raw`](https://docs.rs/ados-sensorhub-proto/latest/ados_sensorhub_proto/fn.decode_raw.html) parses a raw frame. Both check the length and the CRC before touching the payload.
- [`cobs`](https://docs.rs/ados-sensorhub-proto/latest/ados_sensorhub_proto/cobs/index.html) and [`crc`](https://docs.rs/ados-sensorhub-proto/latest/ados_sensorhub_proto/crc/index.html) are usable on their own.

Packet types:

|Type|Packet|Payload|
|---|---|---|
|`0x01`|`Hello`|40 bytes: protocol and firmware version, board ID, capability bits, MCU UID, board name|
|`0x10`|`ImuBatch`|20-byte header plus 1 to 64 samples of 14 bytes (raw gyro, accel, temperature)|
|`0x11`|`Baro`|20 bytes: pressure and temperature|
|`0x12`|`Mag`|24 bytes: field in microtesla|
|`0x20`|`Trigger`|28 bytes: camera trigger sequence, exposure start, mid-exposure time and length|
|`0x30`|`Pps`|24 bytes: PPS edge time, lock state, phase error, frequency correction|
|`0x40`|`GnssRaw`|10-byte header plus up to 1014 receiver bytes|
|`0x41`|`RtcmIn`|2-byte header plus up to 1022 correction bytes|
|`0x50`|`Config`|16 bytes: trigger rate and mode, IMU rate and batch size, outputs, heater target|
|`0x60`|`Health`|40 bytes: uptime, temperatures, input voltage, drop and error counters|

Raw IMU counts convert with `GYRO_LSB_RAD_S` (±2000 dps full scale) and `ACCEL_LSB_M_S2` (±16 g full scale).

## Cargo features

|Feature|Default|What it enables|
|---|---|---|
|`serde`|no|`Serialize`/`Deserialize` on the owned packet structs|

## Part of ADOS

Used in production by the [ADOS Drone Agent](https://github.com/altnautica/ADOSDroneAgent), the onboard software of the ADOS software-defined drone platform.
[All ADOS crates](https://github.com/altnautica/ADOSDroneAgent/tree/main/crates#published-crates) · [Docs](https://docs.altnautica.com) · [Discord](https://discord.gg/uxbvuD4d5q)

## License

Licensed under the [MIT license](./LICENSE).
Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this crate by you shall be licensed as above, without any additional terms or conditions.
