//! `pilot.v1`: the command-authority socket served by `ados-pilot`.
//!
//! Every message is a msgpack map keyed by `op`, carried in a 4-byte
//! big-endian length frame (the same primitive as `state.v2` and the plugin
//! RPC). [`Request`] is what clients send; [`PilotState`] and
//! [`ObstaclesFrame`] are what the pilot publishes, and they are also the
//! bodies of the nav-stream `pilot` and `obstacles` kinds, serialised as JSON
//! with the same field names.
//!
//! Signs on `axes` (`[roll, pitch, thr, yaw]`, each in `[-1, 1]` with the
//! surface's mode mapping and deadzone already applied): pitch + is forward,
//! thr + is climb, yaw + is clockwise, roll + is right.

mod sensors;
mod state;
mod wire;

pub use sensors::*;
pub use state::*;
pub use wire::*;

use serde::{Deserialize, Serialize};

/// Registry version of the `pilot.v1` contract.
pub const PILOT_V1_VERSION: u16 = match crate::contracts::contract_version("pilot.v1") {
    Some(v) => v,
    None => panic!("pilot.v1 is registered in contracts.toml"),
};

/// Largest accepted message body. A `state` message with a full plan, every
/// readiness item and a banner stays far below this.
pub const PILOT_MAX_FRAME: usize = 256 * 1024;

/// The router's `feed` topic for a ground station's mode command toward the
/// flight controller: `{"mode": <custom mode>}` for `SET_MODE` and
/// `DO_SET_MODE`, `{"mode": "rtl"}` for `NAV_RETURN_TO_LAUNCH`,
/// `{"mode": "land"}` for `NAV_LAND`. The pilot tells an operator's mode
/// change from the FC's own (an RC switch or a failsafe) with it.
pub const CLIENT_MODE_TOPIC: &str = "client.mode";

/// One goal from a skill holding the motion lease.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Goal {
    /// `follow`, `orbit`, `position` or `vantage`.
    pub kind: GoalKind,
    pub n: f32,
    pub e: f32,
    pub d: f32,
    #[serde(default)]
    pub vn: f32,
    #[serde(default)]
    pub ve: f32,
    #[serde(default)]
    pub vd: f32,
    /// Absolute yaw in radians, or `None` to leave heading to the pilot.
    #[serde(default)]
    pub yaw: Option<f32>,
    /// Yaw rate in rad/s, used when `yaw` is `None`.
    #[serde(default)]
    pub yaw_rate: Option<f32>,
    /// The subject the skill keeps in view, `[n, e]`.
    #[serde(default)]
    pub subject_ne: Option<[f32; 2]>,
    pub max_speed_mps: f32,
    /// Ring of acceptable detour goals around the subject.
    #[serde(default)]
    pub candidates: Option<Candidates>,
    /// The skill's own obstacle margin; `None` uses `pilot.obstacle_margin_m`.
    #[serde(default)]
    pub margin_m: Option<f32>,
    /// For a `vantage` goal: where the lost subject is predicted to be and
    /// how far the search may go. `n`/`e` are ignored for it; `d` holds
    /// the height.
    #[serde(default)]
    pub vantage: Option<VantageSpec>,
}

impl Goal {
    /// A goal toward `[n, e, d]` at up to `max_speed_mps`, with no
    /// feed-forward, heading, subject, candidates or own margin. Set the
    /// other fields on the returned value.
    pub fn new(kind: GoalKind, ned: [f32; 3], max_speed_mps: f32) -> Self {
        Self {
            kind,
            n: ned[0],
            e: ned[1],
            d: ned[2],
            vn: 0.0,
            ve: 0.0,
            vd: 0.0,
            yaw: None,
            yaw_rate: None,
            subject_ne: None,
            max_speed_mps,
            candidates: None,
            margin_m: None,
            vantage: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum GoalKind {
    Follow,
    Orbit,
    Position,
    /// Fly to a point that sees the lost subject's predicted area.
    Vantage,
}

/// Where a lost subject may be and the bounds of the search for a point
/// that sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct VantageSpec {
    /// Predicted subject position, local NED north/east (m).
    pub predicted_ne: [f32; 2],
    /// Position covariance `[nn, ne, ee]` (m²).
    pub cov_ne: [f32; 3],
    /// Ellipse scale in sigmas for the coverage test.
    pub sigma_k: f32,
    /// Preferred distance from the predicted position (m).
    pub standoff_m: f32,
    /// Vantage points further than this from `anchor_ne` are refused (m).
    pub max_radius_m: f32,
    /// Where the recovery started: the hard bound of the search.
    pub anchor_ne: [f32; 2],
    /// Other places the subject may be (where it stopped, where it turned
    /// back); each counts like an ellipse sample in the coverage test.
    #[serde(default)]
    pub hypotheses_ne: Vec<[f32; 2]>,
}

impl VantageSpec {
    /// A spec with no hypotheses; set them on the returned value.
    pub fn new(
        predicted_ne: [f32; 2],
        cov_ne: [f32; 3],
        sigma_k: f32,
        standoff_m: f32,
        max_radius_m: f32,
        anchor_ne: [f32; 2],
    ) -> Self {
        Self {
            predicted_ne,
            cov_ne,
            sigma_k,
            standoff_m,
            max_radius_m,
            anchor_ne,
            hypotheses_ne: Vec::new(),
        }
    }
}

/// Where the planner may look for a detour goal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidates {
    pub ring_center_ne: [f32; 2],
    pub radius_m: f32,
    pub angle_deg: f32,
    pub angle_span_deg: f32,
}

/// The holder of a motion lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub plugin_id: String,
    pub skill_id: String,
}

/// A client request. Unknown ops (and the `setup.*` family, which the pilot
/// routes to its setup engine) arrive as [`Request::Other`] with the raw map.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Input(Input),
    Release {
        src: String,
        session: u32,
    },
    SessionTake {
        src: String,
        session: u32,
    },
    Hb {
        src: String,
    },
    Action(ActionReq),
    LeaseAcquire {
        holder: Holder,
        kind: String,
    },
    LeaseRelease {
        lease_id: u64,
    },
    Goal {
        lease_id: u64,
        goal: Goal,
    },
    Goto {
        n: f32,
        e: f32,
        d: f32,
        yaw: Option<f32>,
    },
    /// A velocity goal from the router's swarm loop (owner `swarm`).
    SwarmGoal {
        vn: f32,
        ve: f32,
        vd: f32,
        yaw: Option<f32>,
    },
    Subscribe,
    ViewDemand(ViewDemand),
    Feed {
        topic: String,
        payload: rmpv::Value,
    },
    Other {
        op: String,
        body: rmpv::Value,
    },
}

/// The union of live view leases, forwarded by the control front.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ViewDemand {
    /// Served camera id → requested layer ids.
    #[serde(default)]
    pub cameras: std::collections::BTreeMap<String, Vec<String>>,
    /// Served camera id → its projection model.
    #[serde(default)]
    pub models: std::collections::BTreeMap<String, CameraModel>,
}

/// A served camera's projection model, as the AR builder computes it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CameraModel {
    pub frame_w: u32,
    pub frame_h: u32,
    pub hfov_deg: f32,
    pub vfov_deg: f32,
    /// Positive = looking down (mount plus gimbal).
    pub tilt_down_deg: f32,
    /// Camera yaw relative to the body, degrees clockwise.
    pub yaw_deg: f32,
    /// A stabilised gimbal: the tilt holds against the horizon, so vehicle
    /// roll and pitch do not turn the view. A body-mounted camera (`false`,
    /// the default) turns with the airframe.
    #[serde(default)]
    pub stabilised: bool,
}

/// One stick frame from a surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Input {
    pub src: String,
    pub session: u32,
    pub seq: u32,
    pub t_ms: u64,
    pub axes: [f32; 4],
    #[serde(default)]
    pub buttons: u32,
}

/// A named action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionReq {
    pub name: String,
    pub src: String,
    #[serde(default)]
    pub args: Option<serde_json::Value>,
    #[serde(default)]
    pub id: Option<String>,
}
