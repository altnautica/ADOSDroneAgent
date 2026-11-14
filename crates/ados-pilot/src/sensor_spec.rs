//! `cockpit-spec/sensor-health.json`: the thresholds, readiness texts and
//! parameter rules every surface shares, read once at first use.

use std::sync::LazyLock;

use serde::Deserialize;

/// The canonical spec, compiled in.
pub const SPEC_JSON: &str = include_str!("../../../web-shared/src/cockpit-spec/sensor-health.json");

#[derive(Debug, Deserialize)]
pub struct FlowThresholds {
    pub low_quality: u8,
    pub low_hold_ms: u64,
    pub lost_ms: u64,
}

#[derive(Debug, Deserialize)]
pub struct RangeThresholds {
    pub lost_ms: u64,
}

#[derive(Debug, Deserialize)]
pub struct LidarThresholds {
    pub min_hz: f64,
    pub max_hz: f64,
    pub lost_ms: u64,
}

#[derive(Debug, Deserialize)]
pub struct NavImuThresholds {
    pub min_hz: f64,
    pub stale_ms: u64,
}

#[derive(Debug, Deserialize)]
pub struct Thresholds {
    pub flow: FlowThresholds,
    pub range: RangeThresholds,
    pub lidar: LidarThresholds,
    pub nav_imu: NavImuThresholds,
    pub in_flight_flow_warn_ms: u64,
}

#[derive(Debug, Deserialize)]
pub struct Warning {
    pub flow_degraded: String,
    pub flow_lost: String,
}

#[derive(Debug, Deserialize)]
pub struct FlowTexts {
    pub ok: String,
    pub low: String,
    pub lost: String,
    pub absent: String,
}

#[derive(Debug, Deserialize)]
pub struct RangefinderTexts {
    pub ok: String,
    pub absent: String,
}

#[derive(Debug, Deserialize)]
pub struct LidarTexts {
    pub ok: String,
    pub degraded: String,
    pub lost: String,
    /// No lidar expected while obstacle avoidance is on.
    pub absent: String,
}

#[derive(Debug, Deserialize)]
pub struct ParamTexts {
    pub ok: String,
    pub unread: String,
    pub mismatch: String,
}

#[derive(Debug, Deserialize)]
pub struct NavImuTexts {
    pub ok: String,
    pub degraded: String,
}

#[derive(Debug, Deserialize)]
pub struct ReadinessTexts {
    pub flow: FlowTexts,
    pub rangefinder: RangefinderTexts,
    pub lidar: LidarTexts,
    pub sensor_params: ParamTexts,
    pub nav_imu: NavImuTexts,
}

#[derive(Debug, Deserialize)]
pub struct MountSpec {
    pub pos_tolerance_m: f64,
}

#[derive(Debug, Deserialize)]
pub struct Spec {
    pub thresholds: Thresholds,
    pub warning: Warning,
    pub readiness: ReadinessTexts,
    pub mount: MountSpec,
    /// The parameter table, parsed by [`crate::fc_sensor_profile`].
    pub params: Vec<serde_json::Value>,
}

static SPEC: LazyLock<Spec> =
    LazyLock::new(|| serde_json::from_str(SPEC_JSON).expect("sensor-health.json parses"));

/// The parsed spec.
pub fn spec() -> &'static Spec {
    &SPEC
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_spec_parses() {
        let s = super::spec();
        assert_eq!(s.thresholds.flow.low_quality, 128);
        assert!(!s.params.is_empty());
    }
}
