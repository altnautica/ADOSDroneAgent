//! The sensor bus: seam J, `/run/ados/sensors.sock`, contract `sensor.v1`.
//!
//! `ados-clock` binds the socket and brokers it. Producers (the MAVLink
//! router, the sensor hub, the GNSS service) connect and publish frames;
//! consumers (the clock itself, the capture recorder, the GNSS service for hub
//! passthrough bytes, the router and the hub for RTCM) connect, send one
//! [`Kind::Subscribe`] frame naming the kinds they want, and receive every
//! matching frame published by any other client.
//!
//! Wire: a 4-byte big-endian length, then a msgpack map
//! `{v, kind, src, origin, payload}` ([`BusFrame`]). Payload encodings per kind:
//!
//! | kind      | payload                                                  |
//! |-----------|----------------------------------------------------------|
//! | `clock`   | protobuf [`v1::ClockSample`]                             |
//! | `imu`     | protobuf [`v1::ImuBatch`] (body FLU)                     |
//! | `gnss`    | protobuf [`v1::GnssRaw`] (receiver bytes, UBX/NMEA)      |
//! | `trigger` | protobuf [`v1::TriggerRecord`]; `src` is the camera id   |
//! | `baro`    | msgpack [`Baro`]                                         |
//! | `mag`     | msgpack [`Mag`]                                          |
//! | `rtcm`    | raw RTCM3 bytes toward the receivers (FC, hub, rover)    |
//! | `sync`    | msgpack [`SyncObs`], a host↔source time pair             |
//! | `subscribe` | msgpack list of [`Kind`] (client → broker only)        |
//!
//! Every `t_ns` a producer puts on the bus is on the host monotonic clock
//! (`CLOCK_MONOTONIC`, ns); producers convert their source time with their
//! own [`fit::ClockFit`]. The `clock` frames carry the mapping from host
//! monotonic to the session timebase, so a recorder converts once.

pub mod broker;
pub mod client;
pub mod fit;
pub mod frame;
pub mod rig;
pub mod rtcm;
pub mod time;

/// The capture contract `ados.capture.v1` messages, generated from the
/// vendored `.proto` files in `contract/proto`.
pub mod v1 {
    #![allow(clippy::all, missing_docs)]
    include!(concat!(env!("OUT_DIR"), "/ados.capture.v1.rs"));
}

pub use client::{publish_once, Publisher, Subscriber};
pub use frame::{
    decode_body, encode, Baro, BusFrame, FrameError, Kind, Mag, Origin, SyncObs, SyncSource,
};

/// Wire version of a [`BusFrame`]; mirrors the `sensor.v1` row in
/// `contracts.toml`.
pub const SENSOR_BUS_VERSION: u16 = 1;

/// Largest encoded frame body the bus accepts (1 MiB).
pub const MAX_FRAME_BYTES: usize = 1 << 20;

/// The resolved sensor-bus socket path (`ADOS_RUN_DIR` aware).
pub fn socket_path() -> std::path::PathBuf {
    ados_protocol::paths::run_path("sensors")
}
