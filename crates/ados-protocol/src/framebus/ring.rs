//! Shared-memory slot ring: layout, seqlock writes and torn-read-safe reads.

use super::*;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RingError {
    #[error("slot {slot} out of range (ring has {slot_count} slots)")]
    SlotOutOfRange { slot: u32, slot_count: u32 },
    #[error("payload of {len} bytes exceeds slot capacity {cap}")]
    PayloadTooLarge { len: usize, cap: usize },
    #[error("shared region is {got} bytes, layout needs {need}")]
    RegionTooSmall { got: usize, need: usize },
    #[error("slot_count {slot_count} exceeds the header maximum {max}")]
    SlotCountTooLarge { slot_count: u32, max: u32 },
}

/// Memory layout of a single-writer, many-reader frame ring.
///
/// Layout (all integers little-endian):
///
/// ```text
/// [ ring header  ]  HEADER_LEN bytes: magic, version, slot_count, slot_bytes
/// [ slot 0       ]  SLOT_HEADER_LEN + slot_bytes + SLOT_TRAILER_LEN
/// [ slot 1       ]
///   ...
/// ```
///
/// Each slot is `seq_begin:u64 | byte_len:u32 | _pad:u32 | <slot_bytes data> | seq_end:u64`.
/// The writer stores `seq_begin`, then the data and length, then `seq_end`; a
/// reader loads `seq_end`, copies the data, then re-checks `seq_begin` and
/// `seq_end` against the descriptor's `seq`. If either differs the read was torn
/// by a slot recycle and is discarded. Sizing `slot_count` above the number of
/// concurrent consumers plus their read latency makes torn reads rare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingLayout {
    pub slot_count: u32,
    /// Capacity of one slot's pixel region (the largest frame the ring holds).
    pub slot_bytes: u32,
}

impl RingLayout {
    /// "ADV1" — ADOS vision ring, version 1.
    pub const MAGIC: u32 = 0x4144_5631;
    pub const VERSION: u16 = 1;
    pub const HEADER_LEN: usize = 16;
    /// seq_begin (u64) + byte_len (u32) + pad (u32).
    pub const SLOT_HEADER_LEN: usize = 16;
    /// seq_end (u64).
    pub const SLOT_TRAILER_LEN: usize = 8;

    /// Size a ring that holds frames up to `width` x `height` in `format`.
    pub fn for_frame(slot_count: u32, width: u32, height: u32, format: FrameFormat) -> Self {
        RingLayout {
            slot_count,
            slot_bytes: format.frame_bytes(width, height) as u32,
        }
    }

    pub fn slot_stride(&self) -> usize {
        Self::SLOT_HEADER_LEN + self.slot_bytes as usize + Self::SLOT_TRAILER_LEN
    }

    /// Total bytes the shared region must provide.
    pub fn total_len(&self) -> usize {
        Self::HEADER_LEN + self.slot_count as usize * self.slot_stride()
    }

    pub(super) fn slot_offset(&self, slot: u32) -> usize {
        Self::HEADER_LEN + slot as usize * self.slot_stride()
    }

    /// Validate the layout against the on-disk header's field widths. The
    /// header stores `slot_count` in two bytes, so a layout above
    /// [`MAX_SLOT_COUNT`] would truncate on write and the reader's
    /// header-derived layout would name a different slot than the writer used.
    /// Callers validate at ring-creation / engine startup so a misconfigured
    /// depth is rejected loudly rather than silently truncated.
    pub fn validate(&self) -> Result<(), RingError> {
        if self.slot_count > MAX_SLOT_COUNT {
            return Err(RingError::SlotCountTooLarge {
                slot_count: self.slot_count,
                max: MAX_SLOT_COUNT,
            });
        }
        Ok(())
    }

    /// Write the ring header at the front of a freshly created region. Rejects a
    /// `slot_count` the two-byte header field cannot represent so the writer's
    /// slot math (`seq % slot_count`) can never diverge from a reader that
    /// re-derives the layout from the header.
    pub fn write_header(&self, region: &mut [u8]) -> Result<(), RingError> {
        self.validate()?;
        self.check_region(region.len())?;
        region[0..4].copy_from_slice(&Self::MAGIC.to_le_bytes());
        region[4..6].copy_from_slice(&Self::VERSION.to_le_bytes());
        region[6..8].copy_from_slice(&(self.slot_count as u16).to_le_bytes());
        region[8..12].copy_from_slice(&self.slot_bytes.to_le_bytes());
        region[12..16].copy_from_slice(&0u32.to_le_bytes());
        Ok(())
    }

    /// Read the layout a writer recorded in a region's header.
    pub fn read_header(region: &[u8]) -> Option<RingLayout> {
        if region.len() < Self::HEADER_LEN {
            return None;
        }
        let magic = u32::from_le_bytes(region[0..4].try_into().unwrap());
        let version = u16::from_le_bytes(region[4..6].try_into().unwrap());
        if magic != Self::MAGIC || version != Self::VERSION {
            return None;
        }
        let slot_count = u16::from_le_bytes(region[6..8].try_into().unwrap()) as u32;
        let slot_bytes = u32::from_le_bytes(region[8..12].try_into().unwrap());
        Some(RingLayout {
            slot_count,
            slot_bytes,
        })
    }

    pub(super) fn check_region(&self, got: usize) -> Result<(), RingError> {
        let need = self.total_len();
        if got < need {
            return Err(RingError::RegionTooSmall { got, need });
        }
        Ok(())
    }
}

/// Write one frame into `slot` of the ring, stamping it with `seq`.
///
/// The single writer is expected to choose `slot = seq % slot_count` and call
/// this once per captured frame, then publish the matching [`FrameDescriptor`].
pub fn write_slot(
    region: &mut [u8],
    layout: &RingLayout,
    slot: u32,
    seq: u64,
    data: &[u8],
) -> Result<(), RingError> {
    if slot >= layout.slot_count {
        return Err(RingError::SlotOutOfRange {
            slot,
            slot_count: layout.slot_count,
        });
    }
    let cap = layout.slot_bytes as usize;
    if data.len() > cap {
        return Err(RingError::PayloadTooLarge {
            len: data.len(),
            cap,
        });
    }
    layout.check_region(region.len())?;

    let base = layout.slot_offset(slot);
    let data_off = base + RingLayout::SLOT_HEADER_LEN;
    let trailer_off = data_off + cap;

    // Seqlock write order (single writer, many cross-process readers):
    //   1. stamp seq_begin
    //   2. Release fence so the begin stamp is visible before the data copy
    //   3. copy the pixel bytes + length
    //   4. Release fence so the data is visible before seq_end commits
    //   5. stamp seq_end
    // The two fences keep a weakly-ordered CPU (the aarch64 SBC target) from
    // reordering the data copy past either guard, so a reader that observes
    // matching begin/end guards is guaranteed to have observed this frame's
    // bytes, not a torn mix of two frames.
    store_seq_volatile(region, base, seq);
    region[base + 8..base + 12].copy_from_slice(&(data.len() as u32).to_le_bytes());
    region[base + 12..base + 16].copy_from_slice(&0u32.to_le_bytes());
    fence(Ordering::Release);
    region[data_off..data_off + data.len()].copy_from_slice(data);
    fence(Ordering::Release);
    store_seq_volatile(region, trailer_off, seq);
    Ok(())
}

/// Read the frame a [`FrameDescriptor`] points at into `out`, validating the
/// seqlock. `out` is cleared and refilled, reusing its capacity.
///
/// Returns the byte length, or `Ok(None)` if the slot no longer holds
/// `expected_seq` (the writer recycled it); `out` is unspecified then and the
/// caller drops that frame and waits for the next descriptor.
pub fn read_slot_into(
    region: &[u8],
    layout: &RingLayout,
    slot: u32,
    expected_seq: u64,
    out: &mut Vec<u8>,
) -> Result<Option<usize>, RingError> {
    if slot >= layout.slot_count {
        return Err(RingError::SlotOutOfRange {
            slot,
            slot_count: layout.slot_count,
        });
    }
    layout.check_region(region.len())?;

    let cap = layout.slot_bytes as usize;
    let base = layout.slot_offset(slot);
    let data_off = base + RingLayout::SLOT_HEADER_LEN;
    let trailer_off = data_off + cap;

    // Seqlock read order (mirrors the write fences in reverse):
    //   1. load seq_end (the committed marker the writer stamps last)
    //   2. Acquire fence so the byte_len read and the copy cannot be hoisted
    //   3. read byte_len and copy the pixel bytes
    //   4. Acquire fence so the guard re-reads cannot be hoisted above the copy
    //   5. re-read seq_begin + seq_end; a mismatch means the writer recycled
    //      the slot mid-copy (the read was torn) and the frame is dropped.
    let seq_end = load_seq_volatile(region, trailer_off);
    if seq_end != expected_seq {
        return Ok(None);
    }
    fence(Ordering::Acquire);
    // The header read and the copy must not move before the seq_end check nor
    // after the guard re-reads; the volatile guard loads plus fences pin them.
    compiler_fence(Ordering::Acquire);
    let byte_len = u32::from_le_bytes(region[base + 8..base + 12].try_into().unwrap()) as usize;
    if byte_len > cap {
        return Ok(None);
    }
    out.clear();
    out.extend_from_slice(&region[data_off..data_off + byte_len]);
    fence(Ordering::Acquire);
    let seq_begin = load_seq_volatile(region, base);
    let seq_end2 = load_seq_volatile(region, trailer_off);
    if seq_begin != expected_seq || seq_end2 != expected_seq {
        return Ok(None);
    }
    Ok(Some(byte_len))
}

// ---------------------------------------------------------------------------
// Detection + model contracts.
//
// Unlike frames, these payloads are small, so they ride the plugin RPC
// envelope and the `vision.detection` event directly (no shared memory). They
// live here so the engine, the plugin host bridge, and the SDK share one shape.
// ---------------------------------------------------------------------------
