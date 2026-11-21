//! The voxel map in the obstacle model. While a 3D tier's map is live it is
//! the obstacle source: on every new publication the slab the vehicle flies
//! through is read as a planar scan's walls and seen space
//! ([`crate::world::VoxWorld::scan`]) and its occupied columns go into the
//! 2D grid ([`crate::world::VoxWorld::slice`]), so the planner and the brake
//! fly on it unchanged, and the planar scans are left out. A stale map (or
//! a planar-tier one) leaves the planar scans in charge again.

use ados_protocol::voxmap::VoxSnapshot;

use super::Pilot;
use crate::obstacles::{BAND_TOP_M, CELL_M, GROUND_REJECT_M, SCAN_CLOSING_MAX_MPS};
use crate::world::Band;

/// The wall slab reaches the planar scan's swept band over the vehicle
/// ([`BAND_TOP_M`]) ...
const WALL_UP_M: f32 = BAND_TOP_M;
/// ... and this far under it: the body, guards and legs under the sensor,
/// with the vertical error a hold flies with.
const WALL_DOWN_M: f32 = 0.5;
/// Half the height of the slab a column must have been seen free through
/// to count as seen: the airframe's own height.
const SEEN_HALF_M: f32 = 0.2;
/// How far the world scan reads, m: the brake's own reach.
pub const WORLD_SCAN_MAX_M: f32 = 20.0;

impl Pilot {
    /// A new publication of the voxel map, at `now_ms` on the pilot's clock
    /// (the runtime's [`crate::world::VoxFeed`], or the simulation). Taken,
    /// it is swapped in and `snap` comes back with the previous map's
    /// buffers for reuse.
    pub fn on_voxmap(&mut self, snap: &mut VoxSnapshot, now_ms: u64) {
        let local = self.pos(now_ms);
        if self.world.offer(snap, local, now_ms) {
            self.world_obstacles(now_ms);
        }
    }

    /// The fastest closing speed a standing obstacle can show this scan:
    /// the widest of the brake's per-direction trust bounds at the FC's
    /// speed now, or [`SCAN_CLOSING_MAX_MPS`] without a finite one.
    pub(super) fn scan_closing_max(&self, now_ms: u64) -> f32 {
        self.fc
            .fresh_local(now_ms)
            .map(|l| super::control::scan_closing_bound_max(l.vn.hypot(l.ve)))
            .filter(|v| v.is_finite())
            .unwrap_or(SCAN_CLOSING_MAX_MPS)
    }

    /// Read the live 3D map's slab around the vehicle into the obstacle
    /// model. Unknown space within the brake's keep-out (and a cell) of the
    /// vehicle reads seen to there: a forward camera never sees beside the
    /// vehicle, and a seen-space edge inside the keep-out would stop every
    /// path. The sensing speed rule ([`super::control`]'s `sensing`) holds
    /// such a tier to what it saw along its motion.
    fn world_obstacles(&mut self, now_ms: u64) {
        if !self.world.live_3d(now_ms) {
            return;
        }
        let Some(pos) = self.pos(now_ms) else {
            return;
        };
        let ground_d = self.agl(now_ms).map(|a| pos[2] + a);
        let band = |half_up, half_down| {
            Band::around(pos[2], half_up, half_down, ground_d, GROUND_REJECT_M)
        };
        let walls = band(WALL_UP_M, WALL_DOWN_M);
        let seen = band(SEEN_HALF_M, SEEN_HALF_M);
        let yaw = self.yaw(now_ms);
        let bins = self.world.scan(pos, yaw, walls, seen, WORLD_SCAN_MAX_M);
        let mut cells = Vec::new();
        if let Some(w) = walls {
            self.world.slice(w, &mut cells);
        }
        let min_seen_m = super::control::keep_out_m(self.margin()) + CELL_M;
        let closing_max = self.scan_closing_max(now_ms);
        self.obstacles.ingest_world(
            &bins,
            cells,
            [pos[0], pos[1]],
            yaw,
            min_seen_m,
            closing_max,
            now_ms,
        );
        // The leaves over the way, as a planar scan's overhead returns are.
        self.note_overhead(self.agl(now_ms), now_ms);
    }
}
