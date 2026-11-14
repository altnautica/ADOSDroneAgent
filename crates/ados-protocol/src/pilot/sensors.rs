//! Navigation sensor health on the nav-stream `pilot` kind (`sensors`): the
//! optical flow and downward range the flight controller reports, and the
//! scanning lidar. States and thresholds are `cockpit-spec/sensor-health.json`.

use serde::{Deserialize, Serialize};

/// Optical flow state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowState {
    Ok,
    /// Quality below the spec's `low_quality` for `low_hold_ms`.
    Low,
    /// None for `lost_ms` after one was seen.
    Lost,
    /// Never seen.
    #[default]
    Absent,
}

/// Downward rangefinder state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RangeState {
    Ok,
    /// The reading lies outside the sensor's own `[min, max]`.
    OutOfRange,
    Lost,
    #[default]
    Absent,
}

/// Scanning lidar state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LidarState {
    Ok,
    /// Scan rate outside the spec band, or the driver reports warning/error.
    Degraded,
    Lost,
    #[default]
    Absent,
}

/// The lidar driver's own health word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LidarDriverHealth {
    Good,
    Warning,
    Error,
}

/// OPTICAL_FLOW from the flight controller.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FlowHealth {
    pub state: FlowState,
    /// The last quality, 0..=255.
    pub quality: Option<u8>,
    /// Messages per second over the last 2 s.
    pub rate_hz: Option<f64>,
    pub age_ms: Option<u64>,
}

/// The downward (orientation 25) DISTANCE_SENSOR from the flight controller.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RangeHealth {
    pub state: RangeState,
    /// The current reading in metres while it is in range.
    pub height_m: Option<f64>,
    pub age_ms: Option<u64>,
}

/// The scanning lidar the capture service drives.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LidarHealth {
    pub state: LidarState,
    /// Scan arrivals per second over the last 2 s.
    pub rate_hz: Option<f64>,
    pub health: Option<LidarDriverHealth>,
    /// Since the last scan.
    pub age_ms: Option<u64>,
}

/// The `pilot` kind's `sensors`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SensorHealth {
    pub flow: FlowHealth,
    pub range: RangeHealth,
    pub lidar: LidarHealth,
}
