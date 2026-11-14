//! `health` in `GET /api/v1/sensors`: the navigation sensors as the
//! surfaces judge them, `{flow, range, lidar, cameras}`.
//!
//! * `flow` and `range` are the pilot's own verdicts on the optical flow and
//!   the downward rangefinder the flight controller relays (its `sensors`
//!   block on the `pilot` nav kind), with the declared mount from the agent
//!   config `pilot.fc_sensors`. `null` while no fresh pilot frame carries them
//!   (no pilot, or no FC link).
//! * `lidar` is the pilot's verdict too (`sensors.lidar`) while a fresh pilot
//!   frame carries it; without one it is judged here, by the pilot's rules,
//!   from the scans the nav hub sees arrive and the lidar driver's status topic.
//! * `cameras` is [`super::health_cameras`].
//!
//! Thresholds and state words come from the cockpit spec
//! `sensor-health.json`, the file every surface classifies by.

use std::sync::LazyLock;
use std::time::SystemTime;

use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::SensingPaths;
use crate::routes::nav::LidarView;

const SPEC_JSON: &str =
    include_str!("../../../../../web-shared/src/cockpit-spec/sensor-health.json");

/// The spec thresholds this module applies.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Thresholds {
    pub lidar_min_hz: f64,
    pub lidar_max_hz: f64,
    pub lidar_lost_ms: u64,
    pub camera_stale_ms: u64,
    pub camera_lost_ms: u64,
}

pub(crate) static THRESHOLDS: LazyLock<Thresholds> = LazyLock::new(|| {
    let spec: Value = serde_json::from_str(SPEC_JSON).expect("sensor-health.json parses");
    let t = &spec["thresholds"];
    let f = |v: &Value| {
        v.as_f64()
            .expect("sensor-health.json threshold is a number")
    };
    let u = |v: &Value| {
        v.as_u64()
            .expect("sensor-health.json threshold is an integer")
    };
    Thresholds {
        lidar_min_hz: f(&t["lidar"]["min_hz"]),
        lidar_max_hz: f(&t["lidar"]["max_hz"]),
        lidar_lost_ms: u(&t["lidar"]["lost_ms"]),
        camera_stale_ms: u(&t["camera"]["stale_ms"]),
        camera_lost_ms: u(&t["camera"]["lost_ms"]),
    }
});

/// A pilot frame older than this no longer speaks for the FC sensors (the
/// pilot publishes its state several times a second).
pub(crate) const PILOT_FRESH_MS: u64 = 3_000;

/// What the health block is built from besides the sidecars.
#[derive(Debug, Clone, Default)]
pub(crate) struct HealthInputs {
    /// The latest `pilot` nav-kind frame and its age in milliseconds.
    pub pilot: Option<(Value, u64)>,
    pub lidar: LidarView,
    /// The agent config text (`pilot.fc_sensors`, `cameras[]`).
    pub config_text: String,
}

/// The flow and rangefinder mount the operator declared, body FRD metres.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub(crate) struct FcSensors {
    #[serde(default)]
    pub flow: Option<FlowMount>,
    #[serde(default)]
    pub rangefinder: Option<RangeMount>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub(crate) struct FlowMount {
    #[serde(default)]
    pub pos_m: Option<[f64; 3]>,
    #[serde(default)]
    pub orient_yaw_cdeg: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub(crate) struct RangeMount {
    #[serde(default)]
    pub pos_m: Option<[f64; 3]>,
}

impl FcSensors {
    /// `pilot.fc_sensors` from the agent config text; undeclared (all `None`)
    /// when absent or not of the declared shape.
    pub(crate) fn from_config(text: &str) -> Self {
        #[derive(Default, Deserialize)]
        struct Raw {
            #[serde(default)]
            pilot: Option<RawPilot>,
        }
        #[derive(Default, Deserialize)]
        struct RawPilot {
            #[serde(default)]
            fc_sensors: Option<FcSensors>,
        }
        serde_norway::from_str::<Raw>(text)
            .ok()
            .and_then(|r| r.pilot)
            .and_then(|p| p.fc_sensors)
            .unwrap_or_default()
    }

    /// `health.flow.mount`: null unless a flow position is declared.
    fn flow_mount(&self) -> Value {
        match &self.flow {
            Some(FlowMount {
                pos_m: Some(pos),
                orient_yaw_cdeg,
            }) => json!({"yaw_cdeg": orient_yaw_cdeg, "pos_m": pos}),
            _ => Value::Null,
        }
    }

    /// `health.range.mount`: null unless a rangefinder position is declared.
    fn range_mount(&self) -> Value {
        match self.rangefinder.as_ref().and_then(|r| r.pos_m) {
            Some(pos) => json!({"pos_m": pos}),
            None => Value::Null,
        }
    }
}

/// The `health` block.
pub(crate) fn health_body(
    paths: &SensingPaths,
    inputs: &HealthInputs,
    now: SystemTime,
    now_mono_ns: u64,
) -> Value {
    let mount = FcSensors::from_config(&inputs.config_text);
    let pilot = inputs
        .pilot
        .as_ref()
        .filter(|(_, age)| *age <= PILOT_FRESH_MS)
        .and_then(|(data, age)| Some((data.get("sensors")?.as_object()?, *age)));
    let flow = pilot
        .and_then(|(s, age)| {
            pilot_block(
                s.get("flow")?,
                &["ok", "low", "lost", "absent"],
                &["quality", "rate_hz"],
                age,
            )
        })
        .map(|mut f| {
            f.insert("mount".into(), mount.flow_mount());
            Value::Object(f)
        });
    let range = pilot
        .and_then(|(s, age)| {
            pilot_block(
                s.get("range")?,
                &["ok", "out_of_range", "lost", "absent"],
                &["height_m"],
                age,
            )
        })
        .map(|mut r| {
            r.insert("mount".into(), mount.range_mount());
            Value::Object(r)
        });
    let lidar = pilot
        .and_then(|(s, age)| {
            let v = s.get("lidar")?;
            let mut l = pilot_block(v, &["ok", "degraded", "lost", "absent"], &["rate_hz"], age)?;
            let health = v
                .get("health")
                .and_then(Value::as_str)
                .filter(|h| ["good", "warning", "error"].contains(h));
            l.insert("health".into(), json!(health));
            Some(Value::Object(l))
        })
        .unwrap_or_else(|| lidar_block(&inputs.lidar, &THRESHOLDS));
    json!({
        "flow": flow,
        "range": range,
        "lidar": lidar,
        "cameras": super::health_cameras::camera_rows(
            paths,
            &inputs.config_text,
            now,
            now_mono_ns,
            &THRESHOLDS,
        ),
    })
}

/// One pilot sensor verdict, read leniently: `state` must be one of `states`;
/// each of `fields` is kept when it is a number (null otherwise); `age_ms`
/// gains the frame's own age. `None` when the block is not an object with a
/// known state.
fn pilot_block(
    v: &Value,
    states: &[&str],
    fields: &[&str],
    frame_age_ms: u64,
) -> Option<Map<String, Value>> {
    let state = v.get("state")?.as_str().filter(|s| states.contains(s))?;
    let mut out = Map::new();
    out.insert("state".into(), json!(state));
    for f in fields {
        let n = v.get(*f).filter(|n| n.is_number()).cloned();
        out.insert((*f).into(), n.unwrap_or(Value::Null));
    }
    let age = v
        .get("age_ms")
        .and_then(Value::as_u64)
        .map(|a| a.saturating_add(frame_age_ms));
    out.insert("age_ms".into(), json!(age));
    Some(out)
}

/// The lidar verdict without a pilot, by the pilot's own rules: `lost` when
/// the driver reports it disconnected (with or without a device) or the scans
/// stopped for `lost_ms`; `absent` before any scan (even while the driver
/// reports it connected); `degraded` when a measured rate is off the band or
/// the driver reports a warning or error; `ok` otherwise (too few scans to
/// measure a rate is not off the band).
pub(crate) fn lidar_state(v: &LidarView, t: &Thresholds) -> &'static str {
    let status = v.status.as_ref();
    if status.is_some_and(|s| !s.connected) {
        return "lost";
    }
    let Some(age) = v.age_ms else {
        return "absent";
    };
    if age >= t.lidar_lost_ms {
        return "lost";
    }
    let off_rate = v
        .rate_hz
        .is_some_and(|r| r < t.lidar_min_hz || r > t.lidar_max_hz);
    let unhealthy = status
        .and_then(|s| s.health.as_deref())
        .is_some_and(|h| h == "warning" || h == "error");
    if off_rate || unhealthy {
        "degraded"
    } else {
        "ok"
    }
}

fn lidar_block(v: &LidarView, t: &Thresholds) -> Value {
    json!({
        "state": lidar_state(v, t),
        "rate_hz": v.rate_hz,
        "health": v.status.as_ref().and_then(|s| s.health.clone()),
        "age_ms": v.age_ms,
    })
}

#[cfg(test)]
#[path = "health_tests.rs"]
mod tests;
