#![doc = include_str!("../README.md")]
#![warn(missing_docs)]

#[cfg(unix)]
pub mod chrony;
pub mod envelope;
pub mod fit;
