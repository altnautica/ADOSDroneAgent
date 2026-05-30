//! Per-capability probes. Each submodule owns one probe and is filled in
//! independently; the public signatures here are the contract those fills must
//! match.

pub mod cpu;
pub mod serial;
pub mod soc;
pub mod usb;
pub mod video;
