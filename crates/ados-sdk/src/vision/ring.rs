//! The engine's shared-memory frame rings as the SDK maps and reads them.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ados_protocol::framebus::{self, FrameDescriptor, RingLayout};

use super::Frame;

/// Where the engine creates its frame rings.
pub(super) const DEFAULT_SHM_DIR: &str = "/dev/shm";

/// Mapped frame rings under `dir`, keyed by the descriptor's `shm_name`. One
/// ring per camera.
pub(super) struct RingCache {
    dir: PathBuf,
    rings: HashMap<String, MappedRing>,
}

impl RingCache {
    pub(super) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            rings: HashMap::new(),
        }
    }
}

/// One memory-mapped frame ring: the read-only mmap, the layout recorded in its
/// header, and which file it is.
struct MappedRing {
    map: memmap2::Mmap,
    layout: RingLayout,
    /// `(st_dev, st_ino)` of the mapped file, read off the descriptor that was
    /// mapped, so it names exactly the bytes in `map`.
    file_id: (u64, u64),
    /// The highest sequence read out of this mapping.
    last_seq: u64,
}

impl MappedRing {
    /// Read the descriptor's slot through the seqlock, recording its sequence
    /// when the read holds. The returned [`Frame`] owns its pixels, so each read
    /// fills a buffer sized once to the descriptor's byte length.
    fn read(&mut self, descriptor: &FrameDescriptor) -> Option<Vec<u8>> {
        let mut pixels = Vec::with_capacity(descriptor.byte_len as usize);
        framebus::read_slot_into(
            &self.map,
            &self.layout,
            descriptor.slot,
            descriptor.seq,
            &mut pixels,
        )
        .ok()
        .flatten()?;
        self.last_seq = self.last_seq.max(descriptor.seq);
        Some(pixels)
    }

    /// Whether `path` still names the file this mapping holds.
    fn is_named_by(&self, path: &Path) -> bool {
        std::fs::metadata(path).is_ok_and(|m| (m.dev(), m.ino()) == self.file_id)
    }
}

/// Resolve a descriptor to a [`Frame`], mapping the ring on first sight of its
/// `shm_name`. Returns `None` on a torn/stale read, a ring that cannot be
/// mapped, or a layout/region mismatch — the frame is dropped (latest-wins).
///
/// The engine replaces a ring by unlinking its file and creating a new one
/// under the same name — every engine start does, sweeping the last run's
/// rings — and the new ring's sequence starts over at 1. A mapping of the old
/// file stays readable but never again holds the frame being described (or,
/// on a coincidental sequence, holds the previous run's pixels), which left a
/// subscriber silent until its process restarted. So a sequence at or below
/// one this mapping has already served, or a slot it cannot read, is checked
/// against the file the name names now, and a replaced ring is mapped afresh.
/// A live stream pays nothing for this: its sequence only climbs and its reads
/// hold.
pub(super) fn resolve_frame(
    cache: &Mutex<RingCache>,
    descriptor: FrameDescriptor,
) -> Option<Frame> {
    let mut guard = cache.lock().expect("ring cache lock");
    let cache = &mut *guard;
    let path = cache.dir.join(&descriptor.shm_name);
    if let Some(ring) = cache.rings.get_mut(&descriptor.shm_name) {
        let regressed = descriptor.seq <= ring.last_seq;
        if !regressed {
            if let Some(pixels) = ring.read(&descriptor) {
                return Some(Frame { descriptor, pixels });
            }
        }
        if ring.is_named_by(&path) {
            // Still the live ring: the same descriptor resolved again (once per
            // matching subscription), or a slot the writer already recycled.
            let pixels = if regressed {
                ring.read(&descriptor)
            } else {
                None
            }?;
            return Some(Frame { descriptor, pixels });
        }
        cache.rings.remove(&descriptor.shm_name);
    }
    let mut ring = map_ring(&path)?;
    let pixels = ring.read(&descriptor);
    cache.rings.insert(descriptor.shm_name.clone(), ring);
    Some(Frame {
        descriptor,
        pixels: pixels?,
    })
}

/// Map the ring file at `path` read-only and read the ring layout from its
/// header. `None` if the file is missing, cannot be mapped, or has no valid
/// header.
fn map_ring(path: &Path) -> Option<MappedRing> {
    let file = std::fs::File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    // SAFETY: the region is a POSIX shared-memory object the vision engine
    // owns; mapping it read-only is sound. A concurrent writer recycling slots
    // is the expected case and is detected by the per-slot seqlock in
    // `read_slot_into`, which discards any torn read.
    let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;
    let layout = RingLayout::read_header(&map)?;
    Some(MappedRing {
        map,
        layout,
        file_id: (meta.dev(), meta.ino()),
        last_seq: 0,
    })
}
