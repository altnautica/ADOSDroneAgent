#![doc = include_str!("../README.md")]
#![warn(missing_docs)]

pub mod base;
pub mod nmea;
pub mod receiver;
pub mod stream;
pub mod ubx;
pub mod valset;

#[cfg(test)]
mod decode_tests;
#[cfg(any(test, feature = "fixtures"))]
pub mod fixtures;
