#![doc = include_str!("../README.md")]
#![no_std]
#![warn(missing_docs)]

extern crate alloc;
#[cfg(test)]
#[macro_use]
extern crate std;

pub mod bank;
pub mod channels;
pub mod frame;
pub mod link;
pub mod params;
pub mod scale;
pub mod telemetry;
