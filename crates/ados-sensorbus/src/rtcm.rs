//! RTCM3 framing and MAVLink `GPS_RTCM_DATA` fragmentation.
//!
//! `GPS_RTCM_DATA` carries at most 180 bytes. A message of up to 180 bytes
//! goes out whole with `flags = 0` (fragmented bit clear). A longer one is
//! split into 180-byte fragments with
//! `flags = 1 | fragment_id << 1 | (sequence_id & 0x1f) << 3`, and the
//! sequence id advances once per fragmented message. The receiver treats a
//! fragmented message as complete at the first fragment shorter than 180
//! bytes, or once all four fragments arrived, so a message that is an exact
//! multiple of 180 bytes below 720 ends with a zero-length fragment, and 720
//! bytes is four full fragments. This matches the reassembly the
//! flight-controller firmware implements.

/// Payload capacity of one `GPS_RTCM_DATA` message.
pub const FRAGMENT_LEN: usize = 180;
/// Fragments one message may span (two-bit fragment id).
pub const MAX_FRAGMENTS: usize = 4;
/// Longest message the fragments can carry.
pub const MAX_MESSAGE_LEN: usize = FRAGMENT_LEN * MAX_FRAGMENTS;

/// One `GPS_RTCM_DATA` message body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fragment {
    pub flags: u8,
    pub len: u8,
    pub data: [u8; FRAGMENT_LEN],
}

/// A message too long to carry in four fragments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("RTCM message of {0} bytes exceeds {max} bytes", max = MAX_MESSAGE_LEN)]
pub struct TooLong(pub usize);

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

/// Split a byte stream into whole RTCM3 frames (`0xD3`, 6 reserved bits,
/// 10-bit length, payload, 24-bit CRC-24Q). Bytes before a preamble are
/// skipped, and a preamble whose frame fails its CRC (or runs past the end of
/// the stream) is skipped by one byte so the scan resyncs on the next one.
/// Only CRC-valid frames are returned.
pub fn split_rtcm3(bytes: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 6 <= bytes.len() {
        if bytes[i] != PREAMBLE {
            i += 1;
            continue;
        }
        let len = (((bytes[i + 1] & 0x03) as usize) << 8) | bytes[i + 2] as usize;
        let body_end = i + 3 + len;
        let end = body_end + 3;
        if end <= bytes.len() {
            let want =
                u32::from_be_bytes([0, bytes[body_end], bytes[body_end + 1], bytes[body_end + 2]]);
            if crc24q(&bytes[i..body_end]) == want {
                out.push(&bytes[i..end]);
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests;
