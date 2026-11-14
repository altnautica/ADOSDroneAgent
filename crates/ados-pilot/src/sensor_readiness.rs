//! Readiness items for the navigation sensors: `rangefinder`, `flow`,
//! `sensor_params`, `lidar` and `nav_imu` ([`crate::nav_imu`]), worded and
//! graded by `cockpit-spec/sensor-health.json` (`readiness`).
//!
//! The `flow` item doubles as the in-flight flow warning: airborne, a low or
//! lost flow reads ok until it has lasted `in_flight_flow_warn_ms`, then the
//! item fails with the spec's `warning` text, which every surface's status
//! pill shows as `pilot:flow`.

use ados_protocol::pilot::{ControlMode, FlowState, LidarState, Readiness, Severity};

use crate::config::FcSensors;
use crate::fc::FcState;
use crate::fc_sensor_profile::{check, EKF_VELXY};
use crate::sensor_health::LidarTrack;
use crate::sensor_spec::spec;

/// ArduPilot's `EK3_SRCn_VELXY` value for optical flow.
const VELXY_OPTFLOW: f32 = 5.0;

/// What the sensor items read beyond the FC state.
pub struct SensorCtx<'a> {
    pub lidar: &'a LidarTrack,
    /// Obstacle avoidance is on (`pilot.avoid` is not `off`).
    pub avoid_on: bool,
    pub mounts: &'a FcSensors,
    pub nav_imu: &'a crate::nav_imu::NavImuTrack,
}

fn item(id: &str, ok: bool, severity: Severity, msg: impl Into<String>) -> Readiness {
    Readiness {
        id: id.to_owned(),
        ok,
        severity,
        msg: msg.into(),
    }
}

/// The FC is set up for flow, or flow has arrived.
pub fn flow_expected(fc: &FcState) -> bool {
    fc.params.get("FLOW_TYPE").is_some_and(|v| *v != 0.0) || fc.flow_track.seen()
}

/// Any EKF source set takes horizontal velocity from optical flow.
pub fn ekf_fuses_flow(fc: &FcState) -> bool {
    EKF_VELXY.iter().any(|n| {
        fc.params
            .get(*n)
            .is_some_and(|v| (v - VELXY_OPTFLOW).abs() < 0.5)
    })
}

/// Every sensor item for the current state.
pub fn items(fc: &FcState, mode: ControlMode, ctx: &SensorCtx, now_ms: u64) -> Vec<Readiness> {
    let mut out = vec![rangefinder(fc, now_ms)];
    if flow_expected(fc) {
        let fuses = mode == ControlMode::Ados && ekf_fuses_flow(fc);
        out.push(flow(fc, fuses, now_ms));
        out.push(params(fc, fuses, ctx.mounts));
    }
    if ctx.lidar.expected() {
        out.push(lidar(ctx, mode, now_ms));
    } else if ctx.avoid_on {
        let msg = &spec().readiness.lidar.absent;
        out.push(item("lidar", false, Severity::Warn, msg));
    }
    out.extend(ctx.nav_imu.item(mode, now_ms));
    out
}

fn rangefinder(fc: &FcState, now_ms: u64) -> Readiness {
    let t = &spec().readiness.rangefinder;
    if fc.rangefinder_agl(now_ms).is_some() || fc.landed() && fc.rangefinder.is_some() {
        item("rangefinder", true, Severity::Warn, &t.ok)
    } else {
        item("rangefinder", false, Severity::Warn, &t.absent)
    }
}

fn flow(fc: &FcState, fuses: bool, now_ms: u64) -> Readiness {
    let s = spec();
    let t = &s.readiness.flow;
    let lost_sev = if fuses {
        Severity::Block
    } else {
        Severity::Warn
    };
    let state = fc.flow_track.state(now_ms);
    let airborne = fc.armed && !fc.landed();
    if airborne && matches!(state, FlowState::Low | FlowState::Lost) {
        let held = fc
            .flow_track
            .bad_since(now_ms)
            .is_some_and(|b| now_ms.saturating_sub(b) >= s.thresholds.in_flight_flow_warn_ms);
        return match (held, state) {
            (false, _) => item("flow", true, lost_sev, &t.ok),
            (true, FlowState::Low) => item("flow", false, Severity::Warn, &s.warning.flow_degraded),
            (true, _) => item("flow", false, lost_sev, &s.warning.flow_lost),
        };
    }
    match state {
        FlowState::Ok => item("flow", true, lost_sev, &t.ok),
        FlowState::Low => item("flow", false, Severity::Warn, &t.low),
        FlowState::Lost => item("flow", false, lost_sev, &t.lost),
        FlowState::Absent => item("flow", false, lost_sev, &t.absent),
    }
}

fn params(fc: &FcState, fuses: bool, mounts: &FcSensors) -> Readiness {
    let t = &spec().readiness.sensor_params;
    if !fc.params.contains_key("FLOW_TYPE") {
        return item("sensor_params", false, Severity::Warn, &t.unread);
    }
    let failures = check(|n| fc.params.get(n).copied(), mounts);
    let blocks = failures.iter().any(|f| f.severity == Severity::Block);
    let severity = if fuses && (blocks || failures.is_empty()) {
        Severity::Block
    } else {
        Severity::Warn
    };
    if failures.is_empty() {
        return item("sensor_params", true, severity, &t.ok);
    }
    let details: Vec<&str> = failures.iter().map(|f| f.detail.as_str()).collect();
    item(
        "sensor_params",
        false,
        severity,
        format!("{}: {}", t.mismatch, details.join(", ")),
    )
}

fn lidar(ctx: &SensorCtx, mode: ControlMode, now_ms: u64) -> Readiness {
    let t = &spec().readiness.lidar;
    // An expected lidar that stopped holds the pilot in flight whatever the
    // avoid setting, so on the ground it blocks a pilot-flown launch too.
    let lost_sev = if mode == ControlMode::Ados {
        Severity::Block
    } else {
        Severity::Warn
    };
    match ctx.lidar.state(now_ms) {
        LidarState::Ok => item("lidar", true, lost_sev, &t.ok),
        LidarState::Degraded => item("lidar", false, Severity::Warn, &t.degraded),
        LidarState::Lost | LidarState::Absent => item("lidar", false, lost_sev, &t.lost),
    }
}

#[cfg(test)]
mod tests;
