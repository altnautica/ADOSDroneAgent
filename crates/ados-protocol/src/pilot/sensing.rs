//! What the pilot sees obstacles with, how far it sees clear along its
//! motion, and the speed that still stops inside that range.

use serde::{Deserialize, Serialize};

/// The sensing tier of the world model the pilot flies on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SensingTier {
    /// A 3D lidar's voxel map.
    Lidar3d,
    /// A depth camera's voxel map: known only inside the camera's view.
    Depth,
    /// Learned depth from a camera: its seen range counts at 0.6.
    Camera,
    /// A planar lidar's scan (no voxel map, or one built from a planar
    /// lidar alone).
    #[default]
    Planar,
}

impl SensingTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lidar3d => "lidar3d",
            Self::Depth => "depth",
            Self::Camera => "camera",
            Self::Planar => "planar",
        }
    }
}

/// The pilot's `sensing` state: the tier, the seen-clear range along the
/// velocity flown (or asked, from a hover), and the horizontal speed that
/// stops inside it: `v·0.3 + v²/(2·a_brk) ≤ clear − margin`, the camera
/// tier's range taken at 0.6 and the planar tier held to 3 m/s.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Sensing {
    pub tier: SensingTier,
    /// Metres seen clear from the vehicle along its motion.
    pub clear_range_m: f32,
    /// The speed limit that range allows, m/s.
    pub speed_limit_mps: f32,
}

impl Sensing {
    pub fn new(tier: SensingTier, clear_range_m: f32, speed_limit_mps: f32) -> Self {
        Self {
            tier,
            clear_range_m,
            speed_limit_mps,
        }
    }
}

/// A horizontal speed cap the pilot enforces on every setpoint it flies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SpeedCap {
    /// Why: `gnss-denied` while the EKF's horizontal position does not come
    /// from GNSS; `confined-near` in a confined place while an obstacle is
    /// near the clearance limit or the leg flown is unseen; `position-uncertain`
    /// while `pose_uncertainty_m`'s cap is the slowest.
    pub reason: String,
    /// The horizontal speed limit in m/s.
    pub v_h_mps: f32,
}

impl SpeedCap {
    pub fn new(reason: impl Into<String>, v_h_mps: f32) -> Self {
        Self {
            reason: reason.into(),
            v_h_mps,
        }
    }
}
