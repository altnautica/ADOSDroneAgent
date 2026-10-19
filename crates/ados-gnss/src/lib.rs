//! `ados-gnss`: GNSS receivers on a ground station or handheld node.
//!
//! Reads receiver bytes from a serial port, a UDP port or the sensor hub's
//! passthrough, splits them into UBX / NMEA / RTCM3 frames, and reports fix,
//! position, RTK state, PPS lock, the RTK base (survey-in or fixed position)
//! and the moving-baseline heading in `/run/ados/gnss.json`. Serial and UDP
//! bytes are published on the sensor bus as `gnss` frames; a base receiver's
//! RTCM3 output is published as `rtcm` frames for the router to inject toward
//! the paired drones; PPS pulses are published as `sync` pairs.

pub mod base;
pub mod cmdsock;
pub mod config;
pub mod daemon;
pub mod nmea;
pub mod pps;
pub mod receiver;
pub mod service;
pub mod sources;
pub mod status;
pub mod stream;
pub mod ubx;
pub mod valset;

#[cfg(test)]
mod decode_tests;
#[cfg(test)]
pub(crate) mod fixtures;
