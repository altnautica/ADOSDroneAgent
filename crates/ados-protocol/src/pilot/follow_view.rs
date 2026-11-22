//! The viewpoint the pilot's view planner picked for a `follow_intent` goal.

use serde::{Deserialize, Serialize};

/// Where the pilot keeps the subject in view from, while a skill's lease
/// flies a `follow_intent`: the bearing and distance from the subject, the
/// height, and the share of the subject's predicted positions it sees.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FollowView {
    /// Its bearing from the subject, degrees clockwise from north, [0, 360).
    pub bearing_deg: f32,
    /// Its horizontal distance from the subject, metres.
    pub radius_m: f32,
    /// Its height over the ground under the vehicle (over home while no
    /// ground height is known), metres.
    pub height_m: f32,
    /// The weighted share of the subject's predicted positions it sees, 0-1.
    pub visibility: f32,
}

impl FollowView {
    pub fn new(bearing_deg: f32, radius_m: f32, height_m: f32, visibility: f32) -> Self {
        Self {
            bearing_deg,
            radius_m,
            height_m,
            visibility,
        }
    }
}
