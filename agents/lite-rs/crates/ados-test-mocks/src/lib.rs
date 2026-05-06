//! In-process test fixtures for the lite ADOS Drone Agent.
//!
//! This crate is dev-only. It provides:
//!
//!   * [`MockMqttBroker`] — an in-process MQTT v3 broker bound to a
//!     loopback ephemeral port. Built on top of `rumqttd` so the
//!     fixture speaks the same wire format the agent's cloud relay
//!     client expects.
//!
//!   * [`MockRtspServer`] — a minimal RTSP/1.0 server bound to a
//!     loopback ephemeral port. Accepts the standard handshake
//!     (OPTIONS / ANNOUNCE / SETUP / RECORD / PLAY / TEARDOWN) and
//!     captures any RTP frames the client interleaves over the
//!     control connection (RFC 2326 §10.12). The captured frames are
//!     stored as raw bytes; nothing is decoded.
//!
//! Both fixtures bind on `127.0.0.1:0` so multiple instances can run
//! concurrently without port collisions and so the tests do not need
//! any external network access.

pub mod mqtt;
pub mod rtsp;

pub use mqtt::{MockMqttBroker, MockMqttError};
pub use rtsp::{MockRtspError, MockRtspServer};

/// Top-level error type for fixture setup. Each fixture has its own
/// error type that flows up through this enum so callers can match on
/// the concrete cause without depending on the inner crate's error
/// representation.
#[derive(Debug, thiserror::Error)]
pub enum MockError {
    #[error("mqtt broker fixture failed: {0}")]
    Mqtt(#[from] MockMqttError),
    #[error("rtsp server fixture failed: {0}")]
    Rtsp(#[from] MockRtspError),
}
