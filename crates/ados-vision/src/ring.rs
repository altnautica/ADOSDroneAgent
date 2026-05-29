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

use ados_protocol::framebus::{write_slot, FrameDescriptor, FrameFormat, RingError, RingLayout};
use thiserror::Error;

/// The directory `/dev/shm` rings live under. Overridable for tests via
/// `ADOS_SHM_DIR`.
const DEFAULT_SHM_DIR: &str = "/dev/shm";

#[derive(Debug, Error)]
pub enum RingWriterError {
    #[error("ring i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("ring layout: {0}")]
    Layout(#[from] RingError),
    #[error("current time is before the unix epoch")]
    Clock,
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
    pub fn open_or_create(shm_name: &str, layout: RingLayout) -> Result<Self, RingWriterError> {
        let total = layout.total_len();

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
            return Ok(Self {
                shm_name: shm_name.to_string(),
                shm_path: Some(path),
                layout,
                region: Region::Mmap { map, _file: file },
                seq: 1,
            });
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = DEFAULT_SHM_DIR;
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
        let mut w = RingWriter::open_or_create(&name, layout).unwrap();

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
        let mut w = RingWriter::open_or_create(&name, layout).unwrap();

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
        let mut w = RingWriter::open_or_create(&name, layout).unwrap();
        let too_big = vec![0u8; layout.slot_bytes as usize + 1];
        let err = w.write_frame("c", 1, 0, 2, 2, FrameFormat::Rgb24, &too_big);
        assert!(matches!(err, Err(RingWriterError::Layout(_))));
    }
}
