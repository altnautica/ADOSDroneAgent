//! CRSF / ExpressLRS RC control lane for the ADOS agent.
//!
//! Owns the RC transmitter module attached over USB-serial: builds and
//! transmits the packed RC channels frame at a fixed cadence, decodes the
//! telemetry the module returns (link statistics, GPS, battery, attitude,
//! flight mode), carries the module's parameter frames, and derives an honest
//! lane state — the received-side link-statistics proof, never a byte
//! counter, decides whether the link reads up.
//!
//! Ownership is per `radio.crsf.mode`: this lane owns the port only in
//! `crsf_rc`. In `mavlink` mode the module is a plain MAVLink byte pipe the
//! MAVLink router ingests as its FC source, so this service holds off the
//! device and stands by at `ready` (see the binary's module doc for the full
//! split).
//!
//! The frame, channel, telemetry, parameter, scaling, bank and link-state
//! codecs live in the `ados-crsf-codec` crate. Module map here: `sources`
//! (the HID/injection authority merge + TTL), `hid` (the primary-gamepad
//! reader), `transport` (serial + TX/RX tasks), `cmdsock` (the command
//! socket), `sidecar` (the state file), `modules` (the configured RC module
//! set and its per-module status), `reg` (the regulatory posture + bring-up
//! event), `config` + `paths` (wiring).

pub mod cmdsock;
pub mod config;
pub mod daemon;
pub mod hid;
pub mod injector;
pub mod modules;
pub mod paths;
pub mod reg;
pub mod sidecar;
pub mod sources;
pub mod transport;
pub mod watchdog;
