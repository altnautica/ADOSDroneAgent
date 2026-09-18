//! The single-writer frame ring writer.
//!
//! Wraps a shared byte region with the [`ados_protocol::framebus`] ring layout
//! and per-slot seqlock. The engine creates one ring per camera, sized for the
//! largest frame that camera will publish, then calls [`RingWriter::write_frame`]
//! once per captured frame; the returned [`FrameDescriptor`] is what gets
//! published on the `vision.frame` topic.
//!
//! On Linux the region is a `/dev/shm/<shm_name>` file mapped with `memmap2`, so
//! a consumer process maps the same name and reads the slot the descriptor
//! points at. Off Linux (the dev host) the region is a plain heap buffer so the
//! crate still builds and unit-tests; a heap-backed ring is single-process only,
//! which is exactly what the round-trip tests need.

use std::path::PathBuf;

use ados_protocol::framebus::{
    write_slot, FrameDescriptor, FrameFormat, RingError, RingLayout, FRAMEBUS_DESCRIPTOR_VERSION,
};
use thiserror::Error;

/// The directory `/dev/shm` rings live under. Overridable for tests via
/// `ADOS_SHM_DIR`.
const DEFAULT_SHM_DIR: &str = "/dev/shm";

/// Filename prefix every camera ring shares. The sweep and the `tmpfiles.d`
/// drop-in match on it, so it lives in one place.
pub const RING_NAME_PREFIX: &str = "ados-vision-";

/// Default ceiling on ONE camera ring's shared-memory footprint.
///
/// `/dev/shm` is tmpfs: every byte a ring occupies is resident RAM taken from
/// the same pool the encoder, the model and the flight stack draw on. The wire
/// format caps `slot_count` at `u16::MAX`, which is a header-field limit and
/// not a memory budget — 512 slots of 720p rgb24 is ~1.4 GiB, accepted by the
/// header and fatal on a 2 GB board. This is the bound that is actually about
/// memory; the depth is reduced to fit it.
pub const DEFAULT_RING_BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// The shallowest ring the engine will create: one slot being written while a
/// consumer reads the other.
pub const MIN_SLOT_COUNT: u32 = 2;

#[derive(Debug, Error)]
pub enum RingWriterError {
    #[error("ring i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("ring layout: {0}")]
    Layout(#[from] RingError),
    #[error("current time is before the unix epoch")]
    Clock,
    #[error(
        "ring of {slot_count} x {slot_bytes} B needs {need} B of /dev/shm, over the {budget} B budget"
    )]
    OverBudget {
        slot_count: u32,
        slot_bytes: u32,
        need: usize,
        budget: usize,
    },
}

/// The deepest ring of `slot_bytes` slots that fits in `budget_bytes`, never
/// deeper than `requested` and never shallower than [`MIN_SLOT_COUNT`].
///
/// `None` when even the two-slot floor does not fit, which is a frame size the
/// budget cannot hold at all — the caller must refuse rather than silently
/// create a one-slot ring (every read would race the live frame) or map a
/// region that exhausts tmpfs.
pub fn fit_slot_count(requested: u32, slot_bytes: u32, budget_bytes: usize) -> Option<u32> {
    let stride = RingLayout {
        slot_count: 1,
        slot_bytes,
    }
    .slot_stride();
    let usable = budget_bytes.checked_sub(RingLayout::HEADER_LEN)?;
    let fits = (usable / stride.max(1)) as u64;
    if fits < MIN_SLOT_COUNT as u64 {
        return None;
    }
    Some(requested.clamp(MIN_SLOT_COUNT, fits.min(u32::MAX as u64) as u32))
}

/// Unlink every `/dev/shm` ring left behind by a previous run.
///
/// A ring is unlinked by [`RingWriter::drop`] on a clean exit, but `Drop` does
/// not run on SIGKILL, an OOM-kill or an abort, and a camera that re-enumerates
/// under a new `/dev/videoN` mints a ring under a new name — so a stranded ring
/// holds tmpfs RAM until the box reboots. The engine calls this before it opens
/// any ring, so a restart reclaims the last run's memory. Returns how many
/// files were removed. Best-effort: an unreadable directory or an undeletable
/// file must never stop the engine coming up.
pub fn sweep_stale_rings() -> usize {
    let dir = std::env::var("ADOS_SHM_DIR").unwrap_or_else(|_| DEFAULT_SHM_DIR.to_string());
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(RING_NAME_PREFIX) {
            continue;
        }
        if std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        tracing::info!(dir = %dir, removed, "swept stale vision frame rings");
    }
    removed
}

/// The backing store for a ring: a `/dev/shm` mmap on Linux, a heap buffer off
/// it. Both expose a mutable byte slice through [`Region::as_mut_slice`].
enum Region {
    #[cfg(target_os = "linux")]
    Mmap {
        // The file is kept open so the mapping stays valid and the path is
        // unlinked on drop. The map must be declared before the file so it is
        // dropped first.
        map: memmap2::MmapMut,
        _file: std::fs::File,
    },
    #[allow(dead_code)]
    Heap(Vec<u8>),
}

impl Region {
    fn as_mut_slice(&mut self) -> &mut [u8] {
        match self {
            #[cfg(target_os = "linux")]
            Region::Mmap { map, .. } => &mut map[..],
            Region::Heap(v) => &mut v[..],
        }
    }
}

/// A single-writer shared-memory frame ring.
pub struct RingWriter {
    shm_name: String,
    shm_path: Option<PathBuf>,
    layout: RingLayout,
    region: Region,
    /// Next sequence to stamp. Starts at 1 so `seq == 0` never names a live
    /// frame (consumers can treat 0 as "no frame yet").
    seq: u64,
}

impl RingWriter {
    /// Open (or create) the ring named `shm_name`, sized for `layout`, and write
    /// its header.
    ///
    /// On Linux this creates `/dev/shm/<shm_name>`, truncates it to the layout's
    /// total length, maps it read/write, and stamps the header. Off Linux it
    /// allocates a heap region of the same size.
    ///
    /// `budget_bytes` is the hard ceiling on this ring's footprint, refused
    /// here rather than left to the wire format's `u16` slot-count field: that
    /// field bounds what the header can *represent*, not what the board can
    /// hold, and a ring is resident RAM on tmpfs. Callers size the depth with
    /// [`fit_slot_count`] against the same budget, so reaching this error means
    /// a single frame is too large for the budget at all.
    pub fn open_or_create(
        shm_name: &str,
        layout: RingLayout,
        budget_bytes: usize,
    ) -> Result<Self, RingWriterError> {
        let total = layout.total_len();
        if total > budget_bytes {
            return Err(RingWriterError::OverBudget {
                slot_count: layout.slot_count,
                slot_bytes: layout.slot_bytes,
                need: total,
                budget: budget_bytes,
            });
        }

        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let dir = std::env::var("ADOS_SHM_DIR").unwrap_or_else(|_| DEFAULT_SHM_DIR.to_string());
            let path = PathBuf::from(dir).join(shm_name);
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o644)
                .open(&path)?;
            file.set_len(total as u64)?;
            // SAFETY: the file was just sized to `total`; the mapping covers
            // exactly the file and this writer is the only one mutating it.
            let mut map = unsafe { memmap2::MmapMut::map_mut(&file)? };
            layout.write_header(&mut map[..])?;
            Ok(Self {
                shm_name: shm_name.to_string(),
                shm_path: Some(path),
                layout,
                region: Region::Mmap { map, _file: file },
                seq: 1,
            })
        }

        #[cfg(not(target_os = "linux"))]
        {
            let mut buf = vec![0u8; total];
            layout.write_header(&mut buf)?;
            Ok(Self {
                shm_name: shm_name.to_string(),
                shm_path: None,
                layout,
                region: Region::Heap(buf),
                seq: 1,
            })
        }
    }

    /// The `/dev/shm` name a descriptor carries.
    pub fn shm_name(&self) -> &str {
        &self.shm_name
    }

    /// The ring layout (slot count + slot capacity).
    pub fn layout(&self) -> RingLayout {
        self.layout
    }

    /// Write one frame into the next slot and return its descriptor.
    ///
    /// The slot is `seq % slot_count` (latest-wins recycling), the seqlock is
    /// stamped with the new `seq`, and the internal counter advances so the next
    /// call lands on the next slot. `data` must be no larger than the layout's
    /// slot capacity.
    #[allow(clippy::too_many_arguments)]
    pub fn write_frame(
        &mut self,
        camera_id: &str,
        frame_id: u64,
        ts_ms: i64,
        width: u32,
        height: u32,
        format: FrameFormat,
        data: &[u8],
    ) -> Result<FrameDescriptor, RingWriterError> {
        let seq = self.seq;
        let slot = (seq % self.layout.slot_count as u64) as u32;
        write_slot(self.region.as_mut_slice(), &self.layout, slot, seq, data)?;
        self.seq = self.seq.wrapping_add(1).max(1);
        Ok(FrameDescriptor {
            v: FRAMEBUS_DESCRIPTOR_VERSION,
            camera_id: camera_id.to_string(),
            frame_id,
            ts_ms,
            width,
            height,
            format,
            shm_name: self.shm_name.clone(),
            slot,
            seq,
            byte_len: data.len() as u32,
        })
    }
}

impl Drop for RingWriter {
    fn drop(&mut self) {
        // Unlink the /dev/shm file so a restart re-creates a clean ring rather
        // than inheriting a stale one. Best-effort: a failed unlink just leaves
        // the file for the next open to truncate.
        if let Some(path) = &self.shm_path {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Convenience: the current capture timestamp in milliseconds since the unix
/// epoch, the clock the engine stamps frames with.
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::framebus::read_slot;

    #[test]
    fn writes_header_and_round_trips_a_frame() {
        let layout = RingLayout::for_frame(4, 8, 8, FrameFormat::Rgb24);
        let name = format!("ados-vision-test-{}-rt", std::process::id());
        let mut w = RingWriter::open_or_create(&name, layout, DEFAULT_RING_BUDGET_BYTES).unwrap();

        let frame: Vec<u8> = (0..layout.slot_bytes as u8).collect();
        let desc = w
            .write_frame("uvc-0", 1, 123, 8, 8, FrameFormat::Rgb24, &frame)
            .unwrap();

        assert_eq!(desc.camera_id, "uvc-0");
        assert_eq!(desc.frame_id, 1);
        assert_eq!(desc.seq, 1);
        assert_eq!(desc.slot, 1); // 1 % 4
        assert_eq!(desc.byte_len, layout.slot_bytes);

        // Read it back out of the writer's own region via the contract reader.
        let got = read_slot(w.region.as_mut_slice(), &layout, desc.slot, desc.seq).unwrap();
        assert_eq!(got.as_deref(), Some(frame.as_slice()));
    }

    #[test]
    fn seq_advances_and_recycles_slots() {
        let layout = RingLayout::for_frame(2, 4, 4, FrameFormat::Rgb24);
        let name = format!("ados-vision-test-{}-recycle", std::process::id());
        let mut w = RingWriter::open_or_create(&name, layout, DEFAULT_RING_BUDGET_BYTES).unwrap();

        let d1 = w
            .write_frame("c", 1, 0, 4, 4, FrameFormat::Rgb24, &[1; 4])
            .unwrap();
        let d2 = w
            .write_frame("c", 2, 0, 4, 4, FrameFormat::Rgb24, &[2; 4])
            .unwrap();
        let d3 = w
            .write_frame("c", 3, 0, 4, 4, FrameFormat::Rgb24, &[3; 4])
            .unwrap();

        assert_eq!((d1.seq, d1.slot), (1, 1));
        assert_eq!((d2.seq, d2.slot), (2, 0));
        assert_eq!((d3.seq, d3.slot), (3, 1)); // recycled slot 1 from d1

        // The recycled slot now holds frame 3; the old d1 descriptor is torn.
        assert_eq!(
            read_slot(w.region.as_mut_slice(), &layout, 1, d1.seq).unwrap(),
            None
        );
        assert_eq!(
            read_slot(w.region.as_mut_slice(), &layout, 1, d3.seq).unwrap(),
            Some(vec![3; 4])
        );
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let layout = RingLayout::for_frame(2, 2, 2, FrameFormat::Rgb24);
        let name = format!("ados-vision-test-{}-big", std::process::id());
        let mut w = RingWriter::open_or_create(&name, layout, DEFAULT_RING_BUDGET_BYTES).unwrap();
        let too_big = vec![0u8; layout.slot_bytes as usize + 1];
        let err = w.write_frame("c", 1, 0, 2, 2, FrameFormat::Rgb24, &too_big);
        assert!(matches!(err, Err(RingWriterError::Layout(_))));
    }

    #[test]
    fn a_ring_over_the_byte_budget_is_refused_not_allocated() {
        // 512 slots of 720p rgb24 is ~1.4 GiB: accepted by the u16 slot-count
        // header field, fatal on a 2 GB board. The budget is the bound that is
        // actually about memory, so the open is refused rather than mapped.
        let layout = RingLayout::for_frame(512, 1280, 720, FrameFormat::Rgb24);
        let name = format!("ados-vision-test-{}-budget", std::process::id());
        let err = RingWriter::open_or_create(&name, layout, DEFAULT_RING_BUDGET_BYTES);
        assert!(
            matches!(err, Err(RingWriterError::OverBudget { .. })),
            "a ring over the budget must be refused"
        );
        // Nothing was created on the way to the refusal.
        let dir = std::env::var("ADOS_SHM_DIR").unwrap_or_else(|_| DEFAULT_SHM_DIR.to_string());
        assert!(!std::path::Path::new(&dir).join(&name).exists());
    }

    #[test]
    fn fit_slot_count_reduces_depth_to_the_budget_and_refuses_an_unholdable_frame() {
        let frame_1080p_rgb = FrameFormat::Rgb24.frame_bytes(1920, 1080) as u32;
        // A depth that fits is handed back untouched.
        assert_eq!(
            fit_slot_count(4, frame_1080p_rgb, DEFAULT_RING_BUDGET_BYTES),
            Some(4)
        );
        // A depth that does not fit is reduced, never silently accepted.
        let fitted = fit_slot_count(512, frame_1080p_rgb, DEFAULT_RING_BUDGET_BYTES)
            .expect("1080p fits at some depth in the default budget");
        assert!((MIN_SLOT_COUNT..512).contains(&fitted));
        let layout = RingLayout {
            slot_count: fitted,
            slot_bytes: frame_1080p_rgb,
        };
        assert!(layout.total_len() <= DEFAULT_RING_BUDGET_BYTES);
        // A shallow request is still raised to the two-slot floor, so a read
        // never races the single live frame.
        assert_eq!(
            fit_slot_count(1, frame_1080p_rgb, DEFAULT_RING_BUDGET_BYTES),
            Some(MIN_SLOT_COUNT)
        );
        // A frame the budget cannot hold two of is refused outright rather than
        // degraded to a one-slot ring.
        assert_eq!(fit_slot_count(4, frame_1080p_rgb, 8 * 1024 * 1024), None);
    }
}
