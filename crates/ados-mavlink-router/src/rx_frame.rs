//! One frame on the router's internal fan-out, with the host time it arrived.

use bytes::Bytes;

/// A raw MAVLink frame plus `rx_ns`, the host `CLOCK_MONOTONIC` time the
/// router received it: when the FC reader's `read()` returned for a frame off
/// the flight-controller transport, or when the frame was handed to the
/// fan-out for any other source (relayed, demo, a client). The bytes are the
/// frame verbatim; the timestamp travels beside them and reaches clients on
/// the MAVLink socket as the v2 record header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxFrame {
    pub rx_ns: u64,
    pub bytes: Bytes,
}

impl RxFrame {
    pub fn new(rx_ns: u64, bytes: Bytes) -> Self {
        Self { rx_ns, bytes }
    }

    /// Stamp `bytes` with the current host monotonic time.
    pub fn now(bytes: Bytes) -> Self {
        Self::new(ados_sensorbus::time::mono_ns(), bytes)
    }
}

impl std::ops::Deref for RxFrame {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}

impl AsRef<[u8]> for RxFrame {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}
