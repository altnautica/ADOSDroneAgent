//! The pilot's 3D world model: the voxel map Navigation publishes on seam M
//! (`nav.voxmap.v1`, [`ados_protocol::voxmap`]).
//!
//! The runtime reads the map file ([`VoxSource`]) and hands every new
//! publication to the core ([`crate::core::Pilot::on_voxmap`]), which keeps
//! its own copy here: the core stays pure, and the in-process simulation
//! hands it maps the same way. A map is used only while it is live: a whole
//! seqlock copy, tied to the local frame, and its `seq` advanced within
//! [`STALE_MS`] on the pilot's own clock. The writer's `stamp_ns` is ADOS
//! time of another process and is never compared with the pilot's clock.
//!
//! Frames: Navigation's map frame and the FC's local NED are both
//! north-aligned, so they differ by a translation, taken on every new
//! publication from the map's body pose against the vehicle's local
//! position at its arrival. Every query takes and returns local NED.
//!
//! Unknown space is never free: a voxel reads free only when the map
//! observed it free, and nothing outside the window is known.

mod query;
mod scan;
mod source;

pub use query::RayHit;
pub use scan::{ScanBin, WORLD_SCAN_BINS};
pub use source::VoxSource;

use ados_protocol::pilot::SensingTier;
use ados_protocol::voxmap::{VoxSnapshot, VoxTier};

use crate::geo::V2;

/// A map whose `seq` has not advanced for longer than this, ms (pilot
/// clock), is stale: the pilot flies the planar path.
pub const STALE_MS: u64 = 300;

/// A horizontal slab of the map in local down coordinates (NED, so the top
/// is the smaller value).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Band {
    pub top_d: f32,
    pub bottom_d: f32,
}

impl Band {
    /// The slab from `up_m` over local down position `d` to `down_m` under
    /// it, its bottom cut `ground_reject_m` over the ground at local down
    /// `ground_d` when that is known. `None` when the cut leaves nothing.
    pub fn around(
        d: f32,
        up_m: f32,
        down_m: f32,
        ground_d: Option<f32>,
        ground_reject_m: f32,
    ) -> Option<Self> {
        let top_d = d - up_m;
        let bottom_d = ground_d.map_or(d + down_m, |g| (d + down_m).min(g - ground_reject_m));
        (bottom_d > top_d).then_some(Self { top_d, bottom_d })
    }
}

/// The newest live voxel map and its tie to the local frame.
#[derive(Debug, Default)]
pub struct VoxWorld {
    snap: VoxSnapshot,
    /// Map frame minus local NED, metres; `None` until a publication
    /// arrived with the local position known.
    offset: Option<[f32; 3]>,
    /// When the map's `seq` last advanced, pilot ms.
    advanced_ms: Option<u64>,
}

impl VoxWorld {
    /// Take a new publication at `now_ms`, `local` the vehicle's local NED
    /// position now. A snapshot that is not whole, is malformed, or repeats
    /// the `seq` held is refused (`false`). The planes are copied into the
    /// buffers already held, so a steady window size allocates nothing.
    pub fn offer(&mut self, snap: &VoxSnapshot, local: Option<[f32; 3]>, now_ms: u64) -> bool {
        let m = &snap.meta;
        let n = m.voxel_count();
        let sane = snap.valid
            && m.res_m.is_finite()
            && m.res_m > 0.0
            && m.map_from_body.iter().all(|v| v.is_finite())
            && snap.occupancy.len() == n
            && snap.esdf_dm.len() == n;
        if !sane || (self.snap.valid && snap.seq == self.snap.seq) {
            return false;
        }
        self.snap.valid = true;
        self.snap.seq = snap.seq;
        self.snap.meta = snap.meta;
        self.snap.occupancy.clone_from(&snap.occupancy);
        self.snap.esdf_dm.clone_from(&snap.esdf_dm);
        let t = m.map_from_body;
        self.offset = local.map(|v| [t[0] - v[0], t[1] - v[1], t[2] - v[2]]);
        self.advanced_ms = Some(now_ms);
        true
    }

    /// The map is whole, tied to the local frame, and advanced within
    /// [`STALE_MS`] of `now_ms`.
    pub fn live(&self, now_ms: u64) -> bool {
        self.snap.valid
            && self.offset.is_some()
            && self
                .advanced_ms
                .is_some_and(|t| now_ms.saturating_sub(t) <= STALE_MS)
    }

    /// A map has arrived at some point since start: a 3D sensor exists.
    pub fn seen(&self) -> bool {
        self.advanced_ms.is_some()
    }

    /// The sensing tier of the sensors that built the map: a map built from
    /// a planar lidar alone, or of an unknown tier, is the planar tier.
    pub fn tier(&self) -> SensingTier {
        match self.snap.meta.tier {
            VoxTier::Lidar3d => SensingTier::Lidar3d,
            VoxTier::Depth => SensingTier::Depth,
            VoxTier::Camera => SensingTier::Camera,
            VoxTier::Planar | VoxTier::Unknown => SensingTier::Planar,
        }
    }

    /// The map is live and of a 3D tier: it is the obstacle source.
    pub fn live_3d(&self, now_ms: u64) -> bool {
        self.live(now_ms) && self.tier() != SensingTier::Planar
    }

    /// The FC's local estimate stepped by `d` (an EKF position reset) while
    /// the vehicle stayed put: the map stays where it is in the world.
    pub fn shift(&mut self, d: V2) {
        if let Some(o) = self.offset.as_mut() {
            o[0] -= d[0];
            o[1] -= d[1];
        }
    }

    /// The map held, live or not.
    pub fn snapshot(&self) -> &VoxSnapshot {
        &self.snap
    }

    /// Local NED point `p` in continuous window voxel coordinates.
    fn to_window(&self, p: [f32; 3]) -> Option<[f32; 3]> {
        let o = self.offset?;
        let m = &self.snap.meta;
        self.snap
            .valid
            .then(|| [0, 1, 2].map(|a| (p[a] + o[a]) / m.res_m - m.origin_cell[a] as f32))
    }

    /// The local NED horizontal centre of window column `(i, j)`.
    fn column_centre(&self, i: i64, j: i64) -> V2 {
        let o = self.offset.unwrap_or_default();
        let m = &self.snap.meta;
        [
            (m.origin_cell[0] as f32 + i as f32 + 0.5) * m.res_m - o[0],
            (m.origin_cell[1] as f32 + j as f32 + 0.5) * m.res_m - o[1],
        ]
    }
}

#[cfg(test)]
mod tests;
