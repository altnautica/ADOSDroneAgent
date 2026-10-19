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
/// `SCALED_PRESSURE` message id.
pub const MSG_SCALED_PRESSURE: u32 = 29;
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
/// recording, and the IMU fallback state; plus the table last requested.
#[derive(Debug)]
pub struct SensorStreams {
    capture_active: AtomicBool,
    imu: Mutex<ImuStream>,
    /// `(message id, Hz)` of the last request sent (after the `RAW_IMU`
    /// substitution), so a change re-requests at once instead of waiting out
    /// the cadence and the streams it dropped can be switched off.
    last_sent: Mutex<Option<Vec<(u32, f32)>>>,
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

    fn last(&self) -> std::sync::MutexGuard<'_, Option<Vec<(u32, f32)>>> {
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

    /// The table `sent` was requested at `now`; `capture_table` says whether
    /// it asks for the IMU.
    pub fn requested(&self, capture_table: bool, sent: &[(u32, f32)], now: Instant) {
        {
            let mut s = self.imu();
            *s = if capture_table {
                s.requested(now)
            } else {
                ImuStream::Idle
            };
        }
        *self.last() = Some(sent.to_vec());
    }

    /// Whether `table` differs from the last request sent.
    pub fn changed(&self, table: &[(u32, f32)]) -> bool {
        self.last().as_deref() != Some(table)
    }

    /// Message ids the last request streamed that `table` no longer asks for:
    /// each must be switched off at the FC, which otherwise keeps sending it.
    pub fn dropped_by(&self, table: &[(u32, f32)]) -> Vec<u32> {
        self.last().as_deref().map_or_else(Vec::new, |last| {
            last.iter()
                .map(|&(id, _)| id)
                .filter(|id| !table.iter().any(|&(t, _)| t == *id))
                .collect()
        })
    }

    /// The FC sensor streams the last request asked for, by bus stream id
    /// (`fc_imu`, `fc_raw_imu`, `fc_baro`) with their rates.
    pub fn sensor_rates_hz(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut out = serde_json::Map::new();
        for &(id, hz) in self.last().as_deref().unwrap_or_default() {
            let src = match id {
                MSG_HIGHRES_IMU => "fc_imu",
                MSG_RAW_IMU => "fc_raw_imu",
                MSG_SCALED_PRESSURE => "fc_baro",
                _ => continue,
            };
            out.insert(src.into(), serde_json::json!(f64::from(hz)));
        }
        out
    }

    /// The link dropped: the next FC gets a fresh chance at `HIGHRES_IMU`.
    pub fn reset_link(&self) {
        *self.imu() = ImuStream::Idle;
    }
}
