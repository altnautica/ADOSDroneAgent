//! Whether the FC streams `HIGHRES_IMU` or needs `RAW_IMU` instead.
//!
//! When the capture stream table asks for `HIGHRES_IMU` and the flight
//! controller has not sent one within [`HIGHRES_TIMEOUT`] of that first
//! request, the stream request switches to `RAW_IMU` at
//! [`RAW_IMU_FALLBACK_HZ`]. A `HIGHRES_IMU` that turns up later switches back.
//! The state resets with the link (a different FC may answer next) and when
//! the table stops asking for IMU.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a `HIGHRES_IMU` request may go unanswered before falling back.
pub const HIGHRES_TIMEOUT: Duration = Duration::from_secs(5);
/// `HIGHRES_IMU` message id.
pub const MSG_HIGHRES_IMU: u32 = 105;
/// `RAW_IMU` message id.
pub const MSG_RAW_IMU: u32 = 27;
/// Rate requested for `RAW_IMU` once fallen back.
pub const RAW_IMU_FALLBACK_HZ: f32 = 50.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImuStream {
    /// `HIGHRES_IMU` not requested.
    Idle,
    /// Requested at the given time, none seen yet.
    Waiting(Instant),
    /// The FC streams `HIGHRES_IMU`.
    Highres,
    /// The FC never answered; `RAW_IMU` is requested instead.
    RawFallback,
}

impl ImuStream {
    /// A request including `HIGHRES_IMU` was sent at `now`.
    pub fn requested(self, now: Instant) -> Self {
        match self {
            ImuStream::Idle => ImuStream::Waiting(now),
            other => other,
        }
    }

    /// A `HIGHRES_IMU` arrived from the vehicle.
    pub fn highres_seen(self) -> Self {
        match self {
            ImuStream::Idle => ImuStream::Idle,
            _ => ImuStream::Highres,
        }
    }

    /// Advance the timeout at `now`.
    pub fn poll(self, now: Instant) -> Self {
        match self {
            ImuStream::Waiting(since)
                if now.saturating_duration_since(since) >= HIGHRES_TIMEOUT =>
            {
                ImuStream::RawFallback
            }
            other => other,
        }
    }

    /// Whether requests substitute `RAW_IMU` for `HIGHRES_IMU`.
    pub fn substitutes_raw(self) -> bool {
        self == ImuStream::RawFallback
    }
}

/// The sensor-stream inputs the stream cadence reads: whether a capture is
/// recording, and the IMU fallback state.
#[derive(Debug)]
pub struct SensorStreams {
    capture_active: AtomicBool,
    imu: Mutex<ImuStream>,
    /// `(capture table, raw substitution)` of the last request sent, so a
    /// change re-requests at once instead of waiting out the cadence.
    last_sent: Mutex<Option<(bool, bool)>>,
}

impl Default for SensorStreams {
    fn default() -> Self {
        Self {
            capture_active: AtomicBool::new(false),
            imu: Mutex::new(ImuStream::Idle),
            last_sent: Mutex::new(None),
        }
    }
}

impl SensorStreams {
    pub fn capture_active(&self) -> bool {
        self.capture_active.load(Ordering::Relaxed)
    }

    pub fn set_capture_active(&self, active: bool) {
        self.capture_active.store(active, Ordering::Relaxed);
    }

    fn imu(&self) -> std::sync::MutexGuard<'_, ImuStream> {
        self.imu
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn last(&self) -> std::sync::MutexGuard<'_, Option<(bool, bool)>> {
        self.last_sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn imu_state(&self) -> ImuStream {
        *self.imu()
    }

    pub fn highres_seen(&self) {
        let mut s = self.imu();
        *s = s.highres_seen();
    }

    /// Advance the timeout; returns the raw-substitution flag after it.
    pub fn poll(&self, now: Instant) -> bool {
        let mut s = self.imu();
        *s = s.poll(now);
        s.substitutes_raw()
    }

    /// A request was sent at `now` for the given table.
    pub fn requested(&self, capture_table: bool, substitutes_raw: bool, now: Instant) {
        {
            let mut s = self.imu();
            *s = if capture_table {
                s.requested(now)
            } else {
                ImuStream::Idle
            };
        }
        *self.last() = Some((capture_table, substitutes_raw));
    }

    /// Whether `key` differs from the last request sent.
    pub fn changed(&self, key: (bool, bool)) -> bool {
        *self.last() != Some(key)
    }

    /// The link dropped: the next FC gets a fresh chance at `HIGHRES_IMU`.
    pub fn reset_link(&self) {
        *self.imu() = ImuStream::Idle;
    }
}
