//! The moving obstacles the pilot tracks: people, vehicles and anything else
//! its sensors see move, published on the pilot state so every surface can
//! draw them.

use serde::{Deserialize, Serialize};

/// What a tracked mover is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoverKind {
    /// A person (a camera track classed `person`). The pilot keeps every
    /// person at least its separation distance off horizontally unless it
    /// flies high over their head.
    Person,
    /// A road vehicle, bicycle or other machine (a camera track).
    Vehicle,
    /// Something the obstacle sensors see move without a camera class:
    /// kept clear as a person is.
    #[default]
    Unknown,
}

/// Where the pilot learnt of a mover.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoverSource {
    /// A multi-object tracker's track on `vision.detection` with a world
    /// position.
    Vision,
    /// A small cluster of obstacle returns (scan or voxel map) seen moving.
    #[default]
    Scan,
}

/// One tracked mover in the FC's local NED frame, at the state's time (its
/// position predicted on its velocity from when it was last seen).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TrackedMover {
    /// The pilot's id for it, stable while it is tracked.
    pub id: u32,
    pub kind: MoverKind,
    pub source: MoverSource,
    /// North, east, m.
    pub ne: [f64; 2],
    /// North, east velocity, m/s.
    pub vel_ne: [f64; 2],
    /// One-sigma horizontal uncertainty of `ne`, m.
    pub sigma_m: f64,
    /// Since it was last measured, ms.
    pub age_ms: u64,
}

impl TrackedMover {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: u32,
        kind: MoverKind,
        source: MoverSource,
        ne: [f64; 2],
        vel_ne: [f64; 2],
        sigma_m: f64,
        age_ms: u64,
    ) -> Self {
        Self {
            id,
            kind,
            source,
            ne,
            vel_ne,
            sigma_m,
            age_ms,
        }
    }
}
