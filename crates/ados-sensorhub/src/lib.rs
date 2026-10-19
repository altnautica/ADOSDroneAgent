//! The sensor hub service.
//!
//! Reads the hub's packet stream (protocol v1, `ados-sensorhub-proto`) over a
//! USB CDC-ACM port or a TCP socket, maps hub time onto the host monotonic
//! clock, and publishes on the sensor bus:
//!
//! | packet    | bus kind  | src            | payload                         |
//! |-----------|-----------|----------------|---------------------------------|
//! | ImuBatch  | `imu`     | `imu<id>`      | `v1::ImuBatch`, body FLU, SI    |
//! | Trigger   | `trigger` | `cam<i>` per set bit of `cam_mask` | `v1::TriggerRecord` |
//! | Baro      | `baro`    | `baro<id>`     | `Baro`                          |
//! | Mag       | `mag`     | `mag<id>`      | `Mag`, body FLU                 |
//! | GnssRaw   | `gnss`    | `gnss<n>`      | `v1::GnssRaw` (`gnss_forward`)  |
//! | (any)     | `sync`    | `hub`          | `SyncObs` per envelope bucket   |
//!
//! `rtcm` frames on the bus are forwarded to the hub as `RtcmIn`. The link
//! state is written to `/run/ados/sensorhub.json` once a second.

pub mod config;
pub mod convert;
pub mod daemon;
pub mod link;
pub mod rig;
pub mod session;
pub mod state;
pub mod stream;
pub mod timemap;
pub mod transport;
