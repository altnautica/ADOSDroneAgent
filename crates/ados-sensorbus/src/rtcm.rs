//! RTCM3 framing and MAVLink `GPS_RTCM_DATA` fragmentation.
//!
//! `GPS_RTCM_DATA` carries at most 180 bytes. A message shorter than 180
//! bytes goes out whole with `flags = 0` (fragmented bit clear); anything
//! longer (or exactly 180) is split into up to four fragments with
//! `flags = 1 | fragment_id << 1 | (sequence_id & 0x1f) << 3`, and the
//! sequence id advances once per message. This matches the reassembly the
//! flight-controller firmware implements.

/// Payload capacity of one `GPS_RTCM_DATA` message.
pub const FRAGMENT_LEN: usize = 180;
/// Fragments one message may span (two-bit fragment id).
pub const MAX_FRAGMENTS: usize = 4;

/// One `GPS_RTCM_DATA` message body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fragment {
    pub flags: u8,
    pub len: u8,
    pub data: [u8; FRAGMENT_LEN],
}

/// A message too long to carry in four fragments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("RTCM message of {0} bytes exceeds {max} bytes", max = FRAGMENT_LEN * MAX_FRAGMENTS)]
pub struct TooLong(pub usize);

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
        if msg.len() > FRAGMENT_LEN * MAX_FRAGMENTS {
            return Err(TooLong(msg.len()));
        }
        if msg.len() < FRAGMENT_LEN {
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
        Ok(msg
            .chunks(FRAGMENT_LEN)
            .enumerate()
            .map(|(id, chunk)| {
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

/// Number of fragments a byte stream produces after [`split_rtcm3`].
pub fn fragment_count(bytes: &[u8]) -> Result<usize, TooLong> {
    split_rtcm3(bytes)
        .into_iter()
        .map(|m| {
            if m.len() > FRAGMENT_LEN * MAX_FRAGMENTS {
                Err(TooLong(m.len()))
            } else if m.len() < FRAGMENT_LEN {
                Ok(1)
            } else {
                Ok(m.len().div_ceil(FRAGMENT_LEN))
            }
        })
        .sum()
}

/// Split a byte stream into whole RTCM3 frames (`0xD3`, 6 reserved bits,
/// 10-bit length, payload, 24-bit CRC). Bytes that do not form a frame are
/// returned as one trailing chunk so nothing is silently discarded; a stream
/// with no RTCM3 framing at all comes back as a single chunk.
pub fn split_rtcm3(bytes: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= bytes.len() && bytes[i] == 0xD3 {
        let len = (((bytes[i + 1] & 0x03) as usize) << 8) | bytes[i + 2] as usize;
        let end = i + 3 + len + 3;
        if end > bytes.len() {
            break;
        }
        out.push(&bytes[i..end]);
        i = end;
    }
    if i < bytes.len() {
        out.push(&bytes[i..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rtcm3(payload_len: usize) -> Vec<u8> {
        let mut v = vec![0xD3, (payload_len >> 8) as u8 & 0x03, payload_len as u8];
        v.extend((0..payload_len).map(|i| i as u8));
        v.extend([0xAA, 0xBB, 0xCC]);
        v
    }

    #[test]
    fn short_message_is_one_unfragmented_message() {
        let mut f = Fragmenter::default();
        let out = f.fragment(&[1, 2, 3]).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].flags, 0);
        assert_eq!(out[0].len, 3);
        assert_eq!(&out[0].data[..3], &[1, 2, 3]);
        // The sequence did not advance.
        let long = f.fragment(&[0u8; 400]).unwrap();
        assert_eq!(long[0].flags >> 3, 0);
    }

    #[test]
    fn long_message_fragments_with_ids_and_sequence() {
        let mut f = Fragmenter::default();
        let msg: Vec<u8> = (0..500u32).map(|i| i as u8).collect();
        let out = f.fragment(&msg).unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(
            out.iter().map(|x| x.len).collect::<Vec<_>>(),
            vec![180, 180, 140]
        );
        for (i, frag) in out.iter().enumerate() {
            assert_eq!(frag.flags & 1, 1);
            assert_eq!((frag.flags >> 1) & 0x3, i as u8);
            assert_eq!(frag.flags >> 3, 0);
        }
        assert_eq!(&out[2].data[..140], &msg[360..]);
        let next = f.fragment(&msg).unwrap();
        assert_eq!(next[0].flags >> 3, 1);
    }

    #[test]
    fn exactly_180_bytes_is_one_fragmented_message() {
        let mut f = Fragmenter::default();
        let out = f.fragment(&[7u8; 180]).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].flags & 1, 1);
        assert_eq!(out[0].len, 180);
    }

    #[test]
    fn sequence_wraps_at_32() {
        let mut f = Fragmenter::default();
        for _ in 0..32 {
            f.fragment(&[0u8; 200]).unwrap();
        }
        assert_eq!(f.fragment(&[0u8; 200]).unwrap()[0].flags >> 3, 0);
    }

    #[test]
    fn over_720_bytes_is_refused() {
        let mut f = Fragmenter::default();
        assert_eq!(f.fragment(&[0u8; 721]), Err(TooLong(721)));
        assert!(f.fragment(&[0u8; 720]).unwrap().len() == 4);
    }

    #[test]
    fn stream_splits_on_rtcm3_frames_and_counts_fragments() {
        let mut s = rtcm3(20);
        s.extend(rtcm3(300));
        let parts = split_rtcm3(&s);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].len(), 26);
        assert_eq!(parts[1].len(), 306);
        assert_eq!(fragment_count(&s), Ok(3));
        // Non-RTCM bytes stay one chunk.
        assert_eq!(split_rtcm3(&[1, 2, 3]).len(), 1);
    }
}
