//! The confined-place profile on the `pilot` kind: the place class and the
//! route check of the FC mission.

use serde::{Deserialize, Serialize};

/// The kind of place the vehicle flies in, chosen by the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaceClass {
    /// Open sites: the configured obstacle margin, at least 0.5 m.
    #[default]
    Open,
    /// Mines, tunnels and rooms: a margin from the propeller tips, a route
    /// check of the mission against the live occupancy, and a slow cap near
    /// the clearance limit.
    Confined,
}

impl PlaceClass {
    pub fn is_open(&self) -> bool {
        *self == Self::Open
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Confined => "confined",
        }
    }
}

/// The verdict of a mission route check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteState {
    /// Every leg was seen and keeps the required clearance.
    Feasible,
    /// No leg is too tight, but some samples lie in unseen space (or the
    /// pose uncertainty is unknown).
    Unverified,
    /// A seen obstacle is closer to a leg than the required clearance.
    Infeasible,
}

impl RouteState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Feasible => "feasible",
            Self::Unverified => "unverified",
            Self::Infeasible => "infeasible",
        }
    }
}

/// The tightest checked point of a route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pinch {
    /// The leg `from_seq → to_seq`.
    pub from_seq: u32,
    pub to_seq: u32,
    /// The sample in the FC's local NED frame, metres.
    pub ned: [f64; 3],
    /// The distance from the sample to the nearest seen obstacle, metres.
    pub clearance_m: f64,
    pub required_m: f64,
}

/// The route check of the FC mission in a confined place.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteCheck {
    pub state: RouteState,
    /// The clearance every leg needs: the margin plus max(0.1, 3σ).
    pub required_m: f64,
    /// The effective obstacle margin, metres.
    pub margin_m: f64,
    /// The live horizontal pose uncertainty (one sigma, metres); `None`
    /// while unknown.
    pub sigma_m: Option<f64>,
    /// The tightest checked point with a seen obstacle near it; `None` when
    /// no sample had one.
    pub pinch: Option<Pinch>,
    /// `to_seq` of the legs with unseen samples.
    pub unverified_legs: Vec<u32>,
    /// Mission items the check cannot place (no position): the legs to and
    /// from them are not checked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unchecked: Vec<u32>,
    /// The mission has more navigation items than the route the check holds.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}
