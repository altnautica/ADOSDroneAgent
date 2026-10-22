#![doc = include_str!("../README.md")]
#![warn(missing_docs)]

/// Payload capacity of one `GPS_RTCM_DATA` message.
pub const FRAGMENT_LEN: usize = 180;
/// Fragments one message may span (two-bit fragment id).
pub const MAX_FRAGMENTS: usize = 4;
/// Longest message the fragments can carry.
pub const MAX_MESSAGE_LEN: usize = FRAGMENT_LEN * MAX_FRAGMENTS;

/// One `GPS_RTCM_DATA` message body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fragment {
    /// `GPS_RTCM_DATA.flags`: bit 0 fragmented, bits 1-2 fragment id, bits
    /// 3-7 sequence id. Zero for an unfragmented message.
    pub flags: u8,
    /// Bytes of `data` in use.
    pub len: u8,
    /// Fragment bytes, zero padded to [`FRAGMENT_LEN`].
    pub data: [u8; FRAGMENT_LEN],
}

/// A message too long to carry in four fragments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooLong(
    /// Length of the rejected message in bytes.
    pub usize,
);

impl std::fmt::Display for TooLong {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "RTCM message of {} bytes exceeds {} bytes",
            self.0, MAX_MESSAGE_LEN
        )
    }
}

impl std::error::Error for TooLong {}

/// The number of `GPS_RTCM_DATA` messages one RTCM message of `len` bytes
/// becomes.
pub fn message_fragments(len: usize) -> Result<usize, TooLong> {
    if len > MAX_MESSAGE_LEN {
        Err(TooLong(len))
    } else if len <= FRAGMENT_LEN {
        Ok(1)
    } else if len.is_multiple_of(FRAGMENT_LEN) && len < MAX_MESSAGE_LEN {
        // Full fragments only: a zero-length one marks the end.
        Ok(len / FRAGMENT_LEN + 1)
    } else {
        Ok(len.div_ceil(FRAGMENT_LEN))
    }
}

/// Splits RTCM messages into `GPS_RTCM_DATA` fragments, owning the 5-bit
/// sequence id.
#[derive(Debug, Default, Clone)]
pub struct Fragmenter {
    seq: u8,
}

impl Fragmenter {
    /// Fragment one message. The sequence id advances only for a message
    /// that is actually fragmented.
    pub fn fragment(&mut self, msg: &[u8]) -> Result<Vec<Fragment>, TooLong> {
        let count = message_fragments(msg.len())?;
        if msg.len() <= FRAGMENT_LEN {
            let mut data = [0u8; FRAGMENT_LEN];
            data[..msg.len()].copy_from_slice(msg);
            return Ok(vec![Fragment {
                flags: 0,
                len: msg.len() as u8,
                data,
            }]);
        }
        let seq = self.seq & 0x1f;
        self.seq = (self.seq + 1) & 0x1f;
        Ok((0..count)
            .map(|id| {
                let start = (id * FRAGMENT_LEN).min(msg.len());
                let chunk = &msg[start..(start + FRAGMENT_LEN).min(msg.len())];
                let mut data = [0u8; FRAGMENT_LEN];
                data[..chunk.len()].copy_from_slice(chunk);
                Fragment {
                    flags: 1 | ((id as u8 & 0x3) << 1) | (seq << 3),
                    len: chunk.len() as u8,
                    data,
                }
            })
            .collect())
    }
}

/// Number of `GPS_RTCM_DATA` messages a byte stream produces after
/// [`split_rtcm3`].
pub fn fragment_count(bytes: &[u8]) -> Result<usize, TooLong> {
    split_rtcm3(bytes)
        .into_iter()
        .map(|m| message_fragments(m.len()))
        .sum()
}

/// RTCM3 preamble byte.
const PREAMBLE: u8 = 0xD3;

/// CRC-24Q (polynomial 0x1864CFB, initial value 0), the RTCM3 frame check.
pub fn crc24q(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0;
    for &b in bytes {
        crc ^= (b as u32) << 16;
        for _ in 0..8 {
            crc <<= 1;
            if crc & 0x0100_0000 != 0 {
                crc ^= 0x0186_4CFB;
            }
        }
    }
    crc & 0x00FF_FFFF
}

/// Wrap `payload` (at most 1023 bytes) in an RTCM3 frame: preamble, length,
/// payload, CRC-24Q.
pub fn encode_rtcm3(payload: &[u8]) -> Vec<u8> {
    let len = payload.len().min(0x3FF);
    let mut v = Vec::with_capacity(len + 6);
    v.extend([PREAMBLE, (len >> 8) as u8 & 0x03, len as u8]);
    v.extend_from_slice(&payload[..len]);
    let crc = crc24q(&v);
    v.extend(&crc.to_be_bytes()[1..]);
    v
}

/// Scan `bytes` for whole CRC-valid RTCM3 frames (`0xD3`, 6 reserved bits,
/// 10-bit length, payload, 24-bit CRC-24Q), calling `frame` with the range of
/// each in order. Bytes before a preamble are skipped; a preamble whose frame
/// fails its CRC is skipped by one byte so the scan resyncs on the next one.
/// Returns where the incomplete tail starts: the earliest preamble whose frame
/// runs past the end of `bytes` and that no later valid frame overlaps (it may
/// complete with more bytes), else `bytes.len()`. The tail is shorter than
/// one maximum frame (1029 bytes).
fn scan(bytes: &[u8], mut frame: impl FnMut(std::ops::Range<usize>)) -> usize {
    let mut tail: Option<usize> = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != PREAMBLE {
            i += 1;
            continue;
        }
        if i + 3 > bytes.len() {
            tail.get_or_insert(i);
            break;
        }
        let len = (((bytes[i + 1] & 0x03) as usize) << 8) | bytes[i + 2] as usize;
        let body_end = i + 3 + len;
        let end = body_end + 3;
        if end > bytes.len() {
            tail.get_or_insert(i);
            i += 1;
            continue;
        }
        let want =
            u32::from_be_bytes([0, bytes[body_end], bytes[body_end + 1], bytes[body_end + 2]]);
        if crc24q(&bytes[i..body_end]) == want {
            // A valid frame inside a pending candidate's span proves the
            // candidate was a stray preamble.
            tail = None;
            frame(i..end);
            i = end;
        } else {
            i += 1;
        }
    }
    tail.unwrap_or(bytes.len())
}

/// Split one byte buffer into its whole CRC-valid RTCM3 frames (see
/// [`Rtcm3Splitter`] for a stream that arrives in pieces). An incomplete
/// frame at the end is dropped.
pub fn split_rtcm3(bytes: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    scan(bytes, |r| out.push(&bytes[r]));
    out
}

/// Splits an RTCM3 byte stream delivered in arbitrary pieces into whole
/// CRC-valid frames, carrying a partial frame over to the next push. The
/// carried tail is bounded by one maximum frame; junk and corrupt frames are
/// skipped with the same resync as [`split_rtcm3`].
#[derive(Debug, Default, Clone)]
pub struct Rtcm3Splitter {
    buf: Vec<u8>,
}

impl Rtcm3Splitter {
    /// Append `bytes` and return every frame completed by them.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        let buf = &self.buf;
        let tail = scan(buf, |r| out.push(buf[r].to_vec()));
        self.buf.drain(..tail);
        out
    }

    /// Bytes held for a frame not yet complete.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }
}

#[cfg(test)]
mod tests;
