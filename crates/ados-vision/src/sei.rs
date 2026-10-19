//! World Engine frame identity carried in a simulated camera's H.264 stream.
//!
//! The simulator's camera daemon puts one `user_data_unregistered` SEI message
//! (payload type 5) in every encoded frame: a 16-byte UUID followed by a
//! 24-byte payload `{seq u64, sim_time_ns u64, cam u32, rsv u32}`, every field
//! little-endian. That layout is the World Engine writer's
//! (`ADOSWorldEngine/crates/we-video/src/sei.rs`, `FrameId::to_sei`:
//! `b[16..24] = seq.to_le_bytes()`, `b[24..32] = sim_time_ns.to_le_bytes()`,
//! `b[32..36] = cam.to_le_bytes()`, `SEI_LEN = 40`), and the UUID is its
//! `SEI_UUID` (`5745454e-4749-4e45-8000-000000000001`). NVENC writes the
//! message into the bitstream from frame side data (`udu_sei=1`), so it arrives
//! with emulation-prevention bytes applied.
//!
//! [`AnnexBScanner`] splits an Annex-B byte stream into NAL units as it
//! arrives, so a caller can pull the identity out of each frame and forward the
//! bytes to a decoder unchanged.

/// `5745454e-4749-4e45-8000-000000000001`.
pub const WE_SEI_UUID: [u8; 16] = [
    0x57, 0x45, 0x45, 0x4e, 0x47, 0x49, 0x4e, 0x45, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
];

/// UUID plus the 24-byte payload.
pub const WE_SEI_LEN: usize = 40;

/// H.264 NAL unit types this module reads.
pub const NAL_SLICE: u8 = 1;
pub const NAL_IDR: u8 = 5;
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;

/// The identity of one simulated camera frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WeFrameId {
    /// Monotonic per camera; the same sequence the simulated hub stamps on the
    /// trigger that produced the frame.
    pub seq: u64,
    pub sim_time_ns: u64,
    /// Camera index in the simulated rig.
    pub cam: u32,
}

impl WeFrameId {
    /// Parse a `user_data_unregistered` payload; `None` for another UUID or a
    /// short payload.
    pub fn from_payload(p: &[u8]) -> Option<Self> {
        if p.len() < WE_SEI_LEN || p[..16] != WE_SEI_UUID {
            return None;
        }
        let u64_at = |o: usize| u64::from_le_bytes(p[o..o + 8].try_into().unwrap_or_default());
        Some(Self {
            seq: u64_at(16),
            sim_time_ns: u64_at(24),
            cam: u32::from_le_bytes(p[32..36].try_into().unwrap_or_default()),
        })
    }
}

/// NAL unit type of a NAL (header byte first); 0 for an empty slice.
pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |h| h & 0x1f)
}

/// Whether a slice NAL starts a new picture (`first_mb_in_slice == 0`, whose
/// Exp-Golomb code is the single bit `1`).
pub fn starts_picture(nal: &[u8]) -> bool {
    matches!(nal_type(nal), NAL_SLICE | NAL_IDR) && nal.get(1).is_some_and(|b| b & 0x80 != 0)
}

/// Remove emulation-prevention bytes (`00 00 03` → `00 00`) into `out`.
fn unescape(nal: &[u8], out: &mut Vec<u8>) {
    out.clear();
    let mut zeros = 0usize;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
}

/// An `0xFF`-extended SEI value at the front of `r` and the bytes it took.
fn ff_coded(r: &[u8]) -> (usize, usize) {
    let mut v = 0usize;
    for (i, &b) in r.iter().enumerate() {
        v += b as usize;
        if b != 0xff {
            return (v, i + 1);
        }
    }
    (v, r.len())
}

/// The World Engine identity in one SEI NAL (header byte first), if any.
/// `scratch` is reused for the unescaped RBSP.
pub fn frame_id_in_sei(nal: &[u8], scratch: &mut Vec<u8>) -> Option<WeFrameId> {
    if nal_type(nal) != NAL_SEI {
        return None;
    }
    unescape(&nal[1..], scratch);
    let mut r = scratch.as_slice();
    // sei_message()s until the rbsp trailing bits.
    while r.len() > 1 {
        let (kind, a) = ff_coded(r);
        let (size, b) = ff_coded(&r[a..]);
        let body = r.get(a + b..a + b + size)?;
        if kind == 5 {
            if let Some(id) = WeFrameId::from_payload(body) {
                return Some(id);
            }
        }
        r = &r[a + b + size..];
    }
    None
}

/// Splits an Annex-B stream into NAL units as bytes arrive. A NAL is complete
/// once the next start code is seen, so the newest NAL is held back until then.
#[derive(Debug, Default)]
pub struct AnnexBScanner {
    buf: Vec<u8>,
}

impl AnnexBScanner {
    /// Append bytes read off the stream.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete NAL unit (header byte first, start code stripped),
    /// or `None` until more bytes arrive. Bytes before the first start code
    /// are dropped.
    pub fn next_nal(&mut self) -> Option<Vec<u8>> {
        let first = find_start(&self.buf, 0)?;
        let body = first.0 + first.1;
        let Some((next, _)) = find_start(&self.buf, body) else {
            if first.0 > 0 {
                self.buf.drain(..first.0);
            }
            return None;
        };
        // A start code may carry a leading zero byte (4-byte form) that
        // belongs to it, not to the NAL before it.
        let nal = self.buf[body..next].to_vec();
        self.buf.drain(..next);
        Some(nal)
    }
}

/// The offset and length (3 or 4) of the first start code at or after `from`.
fn find_start(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            return Some(if i > from && buf[i - 1] == 0 {
                (i - 1, 4)
            } else {
                (i, 3)
            });
        }
        i += 1;
    }
    None
}

#[cfg(test)]
#[path = "sei_tests.rs"]
mod tests;
