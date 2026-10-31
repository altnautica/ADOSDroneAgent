//! Wire contracts for the local logging and telemetry service.
//!
//! The agent records logs from every process, telemetry history, discrete
//! events, and hardware samples into one durable local store. This module
//! carries the shared types that travel between the producers (Rust services,
//! the Python agent) and the writer, plus the request/response envelope the
//! read API and its clients share.
//!
//! All ingest frames are msgpack with short field-rename keys and a `v` version
//! byte, so a producer and a writer on different versions stay forward
//! compatible: a reader that sees an unknown `v` can skip the frame, and new
//! fields ride in the open `fields`/`tags`/`extra` maps without a schema
//! change. The frames travel over the same 4-byte big-endian length framing the
//! other sockets use ([`crate::frame`]); this module reuses that codec and does
//! not duplicate it.
//!
//! Redaction ([`redact`]) replaces any field whose key looks secret-bearing
//! with its length only. It
//! mirrors the agent's structured-logging redaction so no value is ever written
//! to the store in the clear, regardless of which producer emitted it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::frame;

/// The producer-side `tracing` layer that ships log events to the logging
/// daemon's ingest socket. Behind the `tracing-layer` feature so consumers that
/// do not need it (the daemon itself, build tools, the interactive client) stay
/// lean.
#[cfg(feature = "tracing-layer")]
pub mod layer;

/// The reusable event emitter that ships discrete [`EventFrame`] records from a
/// service to the ingest socket. Unlike the log `layer`, events are a
/// first-class capture path, so this module is always available (not gated on
/// `tracing-layer`).
pub mod emitter;

mod frames;
mod query;
mod redact;
mod sync;
pub use frames::*;
pub use query::*;
pub use redact::*;
pub use sync::*;

/// Maximum ingest frame payload. Log records and hardware snapshots are small;
/// the cap is generous headroom and guards against a runaway producer.
pub const LOGD_MAX_FRAME: usize = 1024 * 1024;

/// The current wire version stamped on every ingest frame.
pub const WIRE_VERSION: u8 = 1;

/// The current read-envelope version reported in [`Meta::v`].
pub const ENVELOPE_VERSION: u8 = 1;

/// Idempotency sentinel: a value already prefixed with this has been redacted
/// and is passed through unchanged.
pub const REDACT_PREFIX: &str = "redacted:";

/// Key suffixes (and exact keys, case-insensitive) that mark a field as
/// secret-bearing. Kept in lockstep with the agent's logging redaction.
pub const SECRET_SUFFIXES: [&str; 5] = ["key", "code", "token", "password", "secret"];

/// Severity levels, matching the integer encoding stored in the log table.
/// `Trace`/`Debug` are the high-volume levels dropped first under backpressure;
/// `Warn` and above are always kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// 0 — most verbose.
    Trace,
    /// 1.
    Debug,
    /// 2.
    Info,
    /// 3.
    Warn,
    /// 4 — most severe.
    Error,
}

impl Level {
    /// The integer encoding stored in the log table.
    pub fn as_u8(self) -> u8 {
        match self {
            Level::Trace => 0,
            Level::Debug => 1,
            Level::Info => 2,
            Level::Warn => 3,
            Level::Error => 4,
        }
    }

    /// Map an integer back to a level, clamping out-of-range values to `Error`.
    pub fn from_u8(n: u8) -> Level {
        match n {
            0 => Level::Trace,
            1 => Level::Debug,
            2 => Level::Info,
            3 => Level::Warn,
            _ => Level::Error,
        }
    }
}

/// Errors raised encoding or decoding an ingest frame.
#[derive(Debug, Error)]
pub enum LogdError {
    #[error("msgpack encode error: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("msgpack decode error: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("framing error: {0}")]
    Frame(#[from] frame::FrameError),
    #[error("unsupported wire version {got} (this build speaks {ours})")]
    Version { got: u8, ours: u8 },
}

/// The msgpack value type carried in the open field/detail/tag maps. Re-exported
/// so a downstream producer can build an [`EventFrame`] detail without taking a
/// direct dependency on `rmpv`.
pub use rmpv::Value;

/// An open msgpack map of extra fields. Values are arbitrary so new telemetry
/// round-trips without a schema change (mirrors the open-state pattern).
pub type Fields = BTreeMap<String, rmpv::Value>;

#[cfg(test)]
mod tests;
