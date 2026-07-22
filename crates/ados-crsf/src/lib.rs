//! CRSF / ExpressLRS RC control lane for the ADOS agent.
//!
//! Owns the RC transmitter module attached over USB-serial: builds and
//! transmits the packed RC channels frame at a fixed cadence, decodes the
//! telemetry the module returns (link statistics, GPS, battery, attitude,
//! flight mode), carries the module's parameter frames, and derives an honest
//! lane state — the received-side link-statistics proof, never a byte
//! counter, decides whether the link reads up.
//!
//! Module map: `frame` (framing + CRC + stream parser), `channels` (the
//! 16×11-bit RC payload), `scale` (input → channel maths), `telemetry` /
//! `params` (typed payload codecs), `bank` (a validated channel-value set),
//! `sources` (the HID/injection authority merge + TTL), `hid` (the primary-
//! gamepad reader), `link` (the lane state machine), `transport` (serial +
//! TX/RX tasks), `cmdsock` (the command socket), `sidecar` (the state file),
//! `config` + `paths` (wiring).

pub mod bank;
pub mod channels;
pub mod cmdsock;
pub mod config;
pub mod frame;
pub mod hid;
pub mod link;
pub mod params;
pub mod paths;
pub mod scale;
pub mod sidecar;
pub mod sources;
pub mod telemetry;
pub mod transport;
pub mod watchdog;
