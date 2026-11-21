//! The voxel map (`nav.voxmap.v1`, seam M) as the free space the subject's
//! predictions are walked through ([`ados_vision::subject::FreeSpace`]).
//!
//! Navigation publishes the map in its own map frame; the subject estimate
//! lives in the flight controller's local NED frame. Both are north-aligned,
//! so on every fresh map the offset between them is taken from the map's
//! body pose against the vehicle's local position, and a local point is
//! shifted by it before it is looked up. A point in an occupied voxel is not
//! free; unknown and free voxels, and points outside the window, are.
//!
//! The map is used only while it is live: the file opened as trusted (group
//! [`VOXMAP_GROUP`], writable by its owner only), its `seq` advanced within
//! [`STALE_MS`] on the engine's clock, and the offset known. Otherwise every
//! point is free, as without a map. A file whose `seq` has not advanced for
//! [`REOPEN_MS`] is reopened, so a Navigation restart that recreated the file
//! is picked up; a missing file is retried at that pace.

use std::path::PathBuf;

use ados_protocol::voxmap::{
    ReadStatus, VoxSnapshot, VoxmapReader, VOXMAP_GROUP, VOXMAP_SHM_PATH, VOX_OCCUPIED,
};
use ados_vision::subject::FreeSpace;

/// A map whose `seq` has not advanced for this long, ms, is not used.
pub(super) const STALE_MS: u64 = 300;

/// The pace of reopening a missing or silent map file, ms.
pub(super) const REOPEN_MS: u64 = 1_000;

/// The voxel map read for the subject's predictions.
pub(super) struct VoxSpace {
    path: PathBuf,
    group: String,
    reader: Option<VoxmapReader>,
    snap: VoxSnapshot,
    /// When the file was last opened (or tried), engine ms.
    opened_ms: Option<u64>,
    /// When the map's `seq` last advanced, engine ms.
    advanced_ms: Option<u64>,
    /// Map frame minus local NED, metres.
    offset: Option<[f32; 3]>,
    live: bool,
}

impl Default for VoxSpace {
    fn default() -> Self {
        Self::new(PathBuf::from(VOXMAP_SHM_PATH), VOXMAP_GROUP)
    }
}

impl VoxSpace {
    pub(super) fn new(path: PathBuf, group: &str) -> Self {
        Self {
            path,
            group: group.to_owned(),
            reader: None,
            snap: VoxSnapshot::default(),
            opened_ms: None,
            advanced_ms: None,
            offset: None,
            live: false,
        }
    }

    /// Take the newest map at `now_ms` (engine clock); `vehicle_local` is the
    /// vehicle's local NED position now, which ties the map's frame to the
    /// local one.
    pub(super) fn refresh(&mut self, now_ms: u64, vehicle_local: Option<[f32; 3]>) {
        let silent = self
            .advanced_ms
            .or(self.opened_ms)
            .is_none_or(|t| now_ms.saturating_sub(t) >= REOPEN_MS);
        let retry = self
            .opened_ms
            .is_none_or(|t| now_ms.saturating_sub(t) >= REOPEN_MS);
        if silent && retry {
            self.opened_ms = Some(now_ms);
            self.reader = VoxmapReader::open_trusted(&self.path, &self.group).ok();
        }
        if let Some(reader) = &mut self.reader {
            match reader.read_into(&mut self.snap) {
                Ok(ReadStatus::Fresh) => {
                    self.advanced_ms = Some(now_ms);
                    let t = &self.snap.meta.map_from_body;
                    self.offset = vehicle_local.map(|v| [t[0] - v[0], t[1] - v[1], t[2] - v[2]]);
                }
                Ok(ReadStatus::Stale | ReadStatus::Torn) => {}
                Err(e) => {
                    tracing::debug!(error = %e, "subject_voxmap_read_failed");
                    self.reader = None;
                    self.snap.valid = false;
                }
            }
        }
        self.live = self.snap.valid
            && self.offset.is_some()
            && self
                .advanced_ms
                .is_some_and(|t| now_ms.saturating_sub(t) < STALE_MS);
    }
}

impl FreeSpace for VoxSpace {
    fn is_free(&self, p: [f32; 3]) -> bool {
        let (true, Some(o)) = (self.live, self.offset) else {
            return true;
        };
        self.snap
            .index_of_point([p[0] + o[0], p[1] + o[1], p[2] + o[2]])
            .is_none_or(|i| self.snap.occupancy[i] != VOX_OCCUPIED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::voxmap::{VoxTier, VoxmapMeta, VoxmapWriter, VOX_FREE};

    const DIMS: [u16; 3] = [40, 40, 10];

    /// A 0.5 m map whose window starts at map (-10, -10, -4) m, the vehicle at
    /// map (2, 3, -5), with one occupied voxel column at map north 5..5.5 m,
    /// east 3..3.5 m.
    fn meta() -> VoxmapMeta {
        VoxmapMeta {
            stamp_ns: 1,
            res_m: 0.5,
            origin_cell: [-20, -20, -8],
            dims: DIMS,
            map_from_body: [2.0, 3.0, -5.0, 1.0, 0.0, 0.0, 0.0],
            tier: VoxTier::Lidar3d,
        }
    }

    fn fill(occ: &mut [u8], _: &mut [u8]) {
        occ.fill(VOX_FREE);
        let (nx, ny) = (usize::from(DIMS[0]), usize::from(DIMS[1]));
        for z in 0..usize::from(DIMS[2]) {
            occ[30 + nx * (26 + ny * z)] = VOX_OCCUPIED;
        }
    }

    /// The name of a group this process belongs to, so the test can write a
    /// trusted file.
    fn own_group() -> Option<String> {
        nix::unistd::Group::from_gid(nix::unistd::getegid())
            .ok()
            .flatten()
            .map(|g| g.name)
    }

    #[test]
    fn a_live_map_blocks_its_occupied_voxels_in_the_local_frame() {
        let Some(group) = own_group() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voxmap");
        let mut w = VoxmapWriter::create(&path, DIMS, Some(&group)).unwrap();
        w.publish(&meta(), fill).unwrap();
        let mut space = VoxSpace::new(path, &group);
        // No map read yet: everything is free.
        assert!(space.is_free([3.2, 0.2, -1.0]));
        // The vehicle is at local (0, 0, -5): the map frame sits 2 m north
        // and 3 m east of the local one.
        space.refresh(1_000, Some([0.0, 0.0, -5.0]));
        assert!(space.live);
        assert!(!space.is_free([3.2, 0.2, -1.0]), "the occupied column");
        assert!(space.is_free([3.2, 1.2, -1.0]), "a free voxel beside it");
        assert!(space.is_free([90.0, 0.0, 0.0]), "outside the window");

        // The seq stops advancing: stale after 300 ms, every point free.
        space.refresh(1_250, Some([0.0, 0.0, -5.0]));
        assert!(space.live);
        space.refresh(1_300, Some([0.0, 0.0, -5.0]));
        assert!(!space.live && space.is_free([3.2, 0.2, -1.0]));
        // A new publication brings it back.
        w.publish(&meta(), fill).unwrap();
        space.refresh(1_400, Some([0.0, 0.0, -5.0]));
        assert!(space.live && !space.is_free([3.2, 0.2, -1.0]));
    }

    #[test]
    fn a_map_without_a_vehicle_position_or_from_an_untrusted_file_is_unused() {
        let Some(group) = own_group() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voxmap");
        let mut w = VoxmapWriter::create(&path, DIMS, Some(&group)).unwrap();
        w.publish(&meta(), fill).unwrap();
        let mut space = VoxSpace::new(path.clone(), &group);
        space.refresh(1_000, None);
        assert!(!space.live && space.is_free([3.2, 0.2, -1.0]));

        let mut wrong = VoxSpace::new(path, "ados-no-such-group-xyz");
        wrong.refresh(1_000, Some([0.0, 0.0, -5.0]));
        assert!(!wrong.live);
        // A missing file is retried at the reopen pace, not on every batch.
        let mut missing = VoxSpace::new(dir.path().join("absent"), &group);
        missing.refresh(1_000, Some([0.0; 3]));
        assert_eq!(missing.opened_ms, Some(1_000));
        missing.refresh(1_500, Some([0.0; 3]));
        assert_eq!(missing.opened_ms, Some(1_000));
        missing.refresh(2_000, Some([0.0; 3]));
        assert_eq!(missing.opened_ms, Some(2_000));
    }
}
