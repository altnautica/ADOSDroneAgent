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
//! The body pose is the vehicle's at the map's `stamp_ns`, the writer's ADOS
//! time. Only on the simulation clock is that comparable with the flight
//! controller's history (the same clock a simulated frame's exposure is read
//! on, see [`super::camera_motion`]), so there the vehicle position at the
//! stamp is used. On wall time ADOS time is per process and not comparable,
//! so the newest position is used: the offset is then wrong by the vehicle's
//! travel between the map's stamp and the batch that first reads it, at most
//! its speed times the writer's build time plus one batch period (about
//! 0.2 s, so about 2 m at 10 m/s, along the direction of travel).
//!
//! The map is used only while it is live: the file opened as trusted (see
//! [`VoxmapReader::open_trusted`]), its `seq` advanced within [`STALE_MS`] on
//! the engine's clock, and the offset known. Otherwise every point is free,
//! as without a map. A file whose `seq` has not advanced for [`REOPEN_MS`] is
//! reopened, so a Navigation restart that recreated the file is picked up; a
//! missing file is retried at that pace.
//!
//! The refresh runs under the map's own lock, never the subject's: a batch
//! takes a [`MapView`] (a shared handle on one immutable publication) before
//! it locks the subject estimate, so file I/O never stalls the estimate or
//! the beacon. Only the occupancy plane is copied. A fresh copy is built in a
//! scratch buffer and swapped in; the previous publication's buffer is taken
//! back as the next scratch once no batch still reads it.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ados_protocol::voxmap::{
    ReadStatus, VoxSnapshot, VoxmapMeta, VoxmapReader, VOXMAP_PATH, VOX_OCCUPIED,
};
use ados_vision::subject::FreeSpace;

use super::VisionEngine;
use crate::depth::feed::FrameTime;

/// A map whose `seq` has not advanced for this long, ms, is not used.
pub(super) const STALE_MS: u64 = 300;

/// The pace of reopening a missing or silent map file, ms.
pub(super) const REOPEN_MS: u64 = 1_000;

/// One publication tied to the local frame.
#[derive(Debug)]
pub(super) struct VoxView {
    snap: VoxSnapshot,
    /// Map frame minus local NED, metres.
    offset: [f32; 3],
}

/// The map one batch reads: a live publication, or none (every point free).
#[derive(Debug, Clone, Default)]
pub(super) struct MapView(Option<Arc<VoxView>>);

impl MapView {
    #[cfg(test)]
    fn is_live(&self) -> bool {
        self.0.is_some()
    }
}

impl FreeSpace for MapView {
    fn is_free(&self, p: [f32; 3]) -> bool {
        let Some(v) = &self.0 else {
            return true;
        };
        let o = v.offset;
        v.snap
            .index_of_point([p[0] + o[0], p[1] + o[1], p[2] + o[2]])
            .is_none_or(|i| v.snap.occupancy[i] != VOX_OCCUPIED)
    }
}

/// The voxel map read for the subject's predictions, behind its own lock.
pub(super) struct VoxSpace {
    inner: Mutex<Refresher>,
}

impl Default for VoxSpace {
    fn default() -> Self {
        Self::new(PathBuf::from(VOXMAP_PATH))
    }
}

impl VoxSpace {
    pub(super) fn new(path: PathBuf) -> Self {
        Self {
            inner: Mutex::new(Refresher {
                path,
                reader: None,
                scratch: VoxSnapshot::default(),
                opened_ms: None,
                advanced_ms: None,
                view: None,
            }),
        }
    }

    /// Take the newest map at `now_ms` (engine clock). `locate` gives the
    /// vehicle's local NED position for a fresh map's header, which ties the
    /// map's frame to the local one; it runs only on a fresh map.
    pub(super) fn refresh(
        &self,
        now_ms: u64,
        locate: impl FnOnce(&VoxmapMeta) -> Option<[f32; 3]>,
    ) -> MapView {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .refresh(now_ms, locate)
    }
}

struct Refresher {
    path: PathBuf,
    reader: Option<VoxmapReader>,
    /// Where the next publication is copied. Between publications it stands
    /// for the one in `view` (its `valid` and `seq`), which is what lets the
    /// reader report nothing new without copying.
    scratch: VoxSnapshot,
    /// When the file was last opened (or tried), engine ms.
    opened_ms: Option<u64>,
    /// When the map's `seq` last advanced, engine ms.
    advanced_ms: Option<u64>,
    view: Option<Arc<VoxView>>,
}

impl Refresher {
    fn refresh(
        &mut self,
        now_ms: u64,
        locate: impl FnOnce(&VoxmapMeta) -> Option<[f32; 3]>,
    ) -> MapView {
        let silent = self
            .advanced_ms
            .or(self.opened_ms)
            .is_none_or(|t| now_ms.saturating_sub(t) >= REOPEN_MS);
        let retry = self
            .opened_ms
            .is_none_or(|t| now_ms.saturating_sub(t) >= REOPEN_MS);
        if silent && retry {
            self.opened_ms = Some(now_ms);
            self.reader = VoxmapReader::open_trusted(&self.path)
                .map_err(|e| tracing::debug!(error = %e, "subject_voxmap_open_failed"))
                .ok();
        }
        if let Some(reader) = &mut self.reader {
            match reader.read_occupancy_into(&mut self.scratch) {
                Ok(ReadStatus::Fresh) => {
                    self.advanced_ms = Some(now_ms);
                    self.swap_in(locate);
                }
                Ok(ReadStatus::Stale | ReadStatus::Torn) => {}
                Err(e) => {
                    tracing::debug!(error = %e, "subject_voxmap_read_failed");
                    self.reader = None;
                    self.view = None;
                }
            }
        }
        let live = self
            .advanced_ms
            .is_some_and(|t| now_ms.saturating_sub(t) < STALE_MS);
        MapView(self.view.clone().filter(|_| live))
    }

    /// Make the fresh copy in the scratch the view, tied to the local frame
    /// by `locate`; without a vehicle position the map is not used.
    fn swap_in(&mut self, locate: impl FnOnce(&VoxmapMeta) -> Option<[f32; 3]>) {
        let seq = self.scratch.seq;
        let t = self.scratch.meta.map_from_body;
        let Some(v) = locate(&self.scratch.meta) else {
            // The copy stays in the scratch, which then stands for it.
            self.view = None;
            return;
        };
        let snap = std::mem::take(&mut self.scratch);
        let offset = [t[0] - v[0], t[1] - v[1], t[2] - v[2]];
        let old = self.view.replace(Arc::new(VoxView { snap, offset }));
        if let Some(old) = old.and_then(|o| Arc::try_unwrap(o).ok()) {
            self.scratch = old.snap;
        }
        self.scratch.valid = true;
        self.scratch.seq = seq;
    }
}

impl VisionEngine {
    /// The voxel map the subject's predictions read at `now_ms` (engine
    /// clock). Refreshed under the map's own lock; call it before locking
    /// the subject estimate.
    pub(super) fn voxmap_view(&self, now_ms: u64) -> MapView {
        let on_sim_clock = self.fc_on_sim_clock();
        let feed = &self.range_feed;
        self.voxmap.refresh(now_ms, |meta| {
            on_sim_clock
                .then(|| feed.position_at(FrameTime::FlightController(meta.stamp_ns as f64 / 1e6)))
                .flatten()
                .or_else(|| feed.position_at(FrameTime::Host(now_ms as f64)))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::voxmap::{VoxTier, VoxmapWriter, VOX_FREE};
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    const DIMS: [u16; 3] = [40, 40, 10];

    /// A 0.5 m map whose window starts at map (-10, -10, -4) m, the vehicle at
    /// map (2, 3, -5), with one occupied voxel column at map north 5..5.5 m,
    /// east 3..3.5 m.
    fn meta(stamp_ns: u64) -> VoxmapMeta {
        VoxmapMeta {
            stamp_ns,
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

    /// The map's path in a directory of its own, as the plugin host lays it
    /// out, under the tempdir made this process's alone (`0700`; a umask of
    /// `002` leaves it group-writable).
    fn map_path(dir: &tempfile::TempDir) -> PathBuf {
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let sub = dir.path().join("voxmap");
        std::fs::create_dir(&sub).unwrap();
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o750)).unwrap();
        sub.join("map-v1")
    }

    const AT_ORIGIN: Option<[f32; 3]> = Some([0.0, 0.0, -5.0]);

    #[test]
    fn a_live_map_blocks_its_occupied_voxels_in_the_local_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = map_path(&dir);
        let mut w = VoxmapWriter::create(&path, DIMS, None).unwrap();
        w.publish(&meta(1), fill).unwrap();
        let space = VoxSpace::new(path);
        // No map read yet: everything is free.
        assert!(MapView::default().is_free([3.2, 0.2, -1.0]));
        // The vehicle is at local (0, 0, -5): the map frame sits 2 m north
        // and 3 m east of the local one.
        let view = space.refresh(1_000, |_| AT_ORIGIN);
        assert!(view.is_live());
        assert!(!view.is_free([3.2, 0.2, -1.0]), "the occupied column");
        assert!(view.is_free([3.2, 1.2, -1.0]), "a free voxel beside it");
        assert!(view.is_free([90.0, 0.0, 0.0]), "outside the window");

        // The seq stops advancing: stale after 300 ms, every point free.
        assert!(space.refresh(1_250, |_| AT_ORIGIN).is_live());
        let stale = space.refresh(1_300, |_| AT_ORIGIN);
        assert!(!stale.is_live() && stale.is_free([3.2, 0.2, -1.0]));
        // A new publication brings it back.
        w.publish(&meta(2), fill).unwrap();
        let back = space.refresh(1_400, |_| AT_ORIGIN);
        assert!(back.is_live() && !back.is_free([3.2, 0.2, -1.0]));
    }

    /// A batch's view is one publication: a newer map swapped in while the
    /// batch still holds its view leaves that view as it was, and the
    /// position used is the one `locate` gives for the newer map's header.
    #[test]
    fn a_held_view_keeps_its_publication_while_a_new_one_is_swapped_in() {
        let dir = tempfile::tempdir().unwrap();
        let path = map_path(&dir);
        let mut w = VoxmapWriter::create(&path, DIMS, None).unwrap();
        w.publish(&meta(10), fill).unwrap();
        let space = VoxSpace::new(path);
        let held = space.refresh(1_000, |_| AT_ORIGIN);
        assert!(!held.is_free([3.2, 0.2, -1.0]));

        w.publish(&meta(20), |occ, _| occ.fill(VOX_FREE)).unwrap();
        let mut seen_stamp = None;
        let newer = space.refresh(1_100, |m| {
            seen_stamp = Some(m.stamp_ns);
            AT_ORIGIN
        });
        assert_eq!(seen_stamp, Some(20));
        assert!(newer.is_free([3.2, 0.2, -1.0]), "the newer map is clear");
        assert!(
            !held.is_free([3.2, 0.2, -1.0]),
            "the held view is unchanged"
        );

        // Nothing new: `locate` is not asked again and the view stays.
        let again = space.refresh(1_150, |_| panic!("no fresh map to locate"));
        assert!(again.is_live());
    }

    #[test]
    fn a_map_without_a_vehicle_position_or_from_an_untrusted_file_is_unused() {
        let dir = tempfile::tempdir().unwrap();
        let path = map_path(&dir);
        let mut w = VoxmapWriter::create(&path, DIMS, None).unwrap();
        w.publish(&meta(1), fill).unwrap();
        let space = VoxSpace::new(path.clone());
        let view = space.refresh(1_000, |_| None);
        assert!(!view.is_live() && view.is_free([3.2, 0.2, -1.0]));

        // Writable beyond its owner: refused.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        let wrong = VoxSpace::new(path);
        assert!(!wrong.refresh(1_000, |_| AT_ORIGIN).is_live());

        // A missing file is retried at the reopen pace, not on every batch.
        let missing = VoxSpace::new(dir.path().join("voxmap").join("absent"));
        let opened = |s: &VoxSpace| s.inner.lock().unwrap().opened_ms;
        missing.refresh(1_000, |_| AT_ORIGIN);
        assert_eq!(opened(&missing), Some(1_000));
        missing.refresh(1_500, |_| AT_ORIGIN);
        assert_eq!(opened(&missing), Some(1_000));
        missing.refresh(2_000, |_| AT_ORIGIN);
        assert_eq!(opened(&missing), Some(2_000));
    }

    /// A FIFO planted at the map's path costs one prompt refused open.
    #[test]
    fn a_fifo_at_the_path_does_not_block_the_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = map_path(&dir);
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let live = VoxSpace::new(path).refresh(1_000, |_| AT_ORIGIN).is_live();
            let _ = tx.send(live);
        });
        let live = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the refresh blocked on the FIFO");
        assert!(!live);
    }

    /// The refresh never waits on the subject estimate's lock: it completes
    /// while another holder keeps that lock.
    #[test]
    fn the_refresh_does_not_take_the_subject_lock() {
        let engine = VisionEngine::new(Box::new(crate::backend::MockBackend), 4);
        let held = engine.subject_runtime();
        let e = engine.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = e.voxmap_view(1_000);
            let _ = tx.send(());
        });
        rx.recv_timeout(Duration::from_secs(2))
            .expect("the refresh waited on the subject lock");
        drop(held);
    }
}
