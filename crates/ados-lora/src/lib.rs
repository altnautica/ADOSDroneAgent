//! LoRa beacon and mesh lane for the ADOS agent.
//!
//! A low-duty lane only: position beacons, short status reports and text,
//! never video, corrections or a control loop. Every frame passes the
//! per-region governor before it reaches a radio.
//!
//! Module map: `regions` (the compiled band-plan table and region
//! resolution), `band` (what the fitted radio can tune and whether it meets a
//! plan), `airtime` (time on air), `ledger` (the persisted charge-then-send
//! airtime ledger), `governor` (the admit pipeline: region, module band,
//! power, dwell, duty, listen-before-talk), `frame` (the lane's frame and
//! beacon codec), `status` (the REST/sidecar body), `config` (the `lora:`
//! block), and, with the `daemon` feature, `transport` (E22, Meshtastic,
//! loopback) and `daemon` (the service).

pub mod airtime;
pub mod band;
pub mod config;
pub mod frame;
pub mod governor;
pub mod ledger;
pub mod paths;
pub mod regions;
pub mod status;
pub mod validate;

#[cfg(feature = "daemon")]
pub mod daemon;
#[cfg(feature = "daemon")]
pub mod transport;
