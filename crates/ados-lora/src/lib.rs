//! LoRa beacon and mesh lane for the ADOS agent.
//!
//! A low-duty lane only: position beacons, short status reports and text,
//! never video, corrections or a control loop. Every frame passes the
//! per-region governor (the `ados-lora-governor` crate) before it reaches a
//! radio.
//!
//! Module map: `frame` (the lane's frame and beacon codec), `status` (the
//! REST/sidecar body), `config` (the `lora:` block), `validate` (proposed
//! config checks), and, with the `daemon` feature, `transport` (E22,
//! Meshtastic, loopback) and `daemon` (the service).

pub mod config;
pub mod frame;
pub mod paths;
pub mod status;
pub mod validate;

#[cfg(feature = "daemon")]
pub mod daemon;
#[cfg(feature = "daemon")]
pub mod transport;
