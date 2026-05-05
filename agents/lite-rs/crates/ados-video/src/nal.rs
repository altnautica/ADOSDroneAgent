//! H.264 Annex-B byte-stream scanner. Used by both the libcamera
//! subprocess backend and the V4L2 backend to walk a stream of bytes
//! and surface complete NAL units one at a time.
//!
//! The scanner buffers partial input across calls so a chunked read
//! that splits a NAL unit across a buffer boundary still emits a
//! single [`Vec<u8>`] for that unit when the next chunk lands.
//!
//! Annex-B start codes are either 3 bytes (`00 00 01`) or 4 bytes
//! (`00 00 00 01`). The scanner handles both.

use bytes::BytesMut;

/// Streaming Annex-B scanner. Push raw bytes via [`Self::push`], then
/// drain emitted NAL units via [`Self::next_unit`] until it returns
/// `None`.
#[derive(Debug, Default)]
pub struct AnnexBScanner {
    buf: BytesMut,
}

impl AnnexBScanner {
    /// Append bytes to the scanner's internal buffer.
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Pull the next complete NAL unit out of the buffer. Returns
    /// `None` when a complete unit is not yet available; the caller
    /// should call [`Self::push`] again with more bytes and retry.
    ///
    /// The returned vector includes the leading Annex-B start code so
    /// downstream consumers (RTSP packetizer, disk recorder) see the
    /// same byte stream that arrived from the encoder.
    pub fn next_unit(&mut self) -> Option<Vec<u8>> {
        // Find the first start code. If none, nothing to do until more
        // bytes land.
        let first = find_start_code(&self.buf, 0)?;
        // Find the next start code after the first. If absent, the
        // current NAL unit is still ongoing — wait for more bytes.
        let next = find_start_code(&self.buf, first.end);
        let end = match next {
            Some(s) => s.start,
            None => return None,
        };
        let unit = self.buf[first.start..end].to_vec();
        // Drop the consumed prefix so the buffer does not grow without
        // bound. `BytesMut::advance` here would orphan the leading
        // window into the upcoming start code; using `split_to` rebases
        // cleanly.
        let _ = self.buf.split_to(end);
        Some(unit)
    }

    /// Quick check: does the buffer contain at least one NAL unit
    /// whose header byte indicates a keyframe (IDR / SPS / PPS)?
    /// Used by the V4L2 backend to mark dequeued buffers without
    /// having to copy the full byte stream into the scanner.
    pub fn contains_keyframe(&mut self, data: &[u8]) -> bool {
        let mut i = 0usize;
        while let Some(s) = find_start_code(data, i) {
            let header_index = s.end;
            if let Some(byte) = data.get(header_index) {
                if is_keyframe_unit(*byte) {
                    return true;
                }
            }
            i = s.end;
        }
        false
    }
}

/// Span of an Annex-B start code in a byte buffer. `start` is the
/// index of the first `0x00` byte; `end` is the index immediately
/// after the trailing `0x01`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartCode {
    pub start: usize,
    pub end: usize,
}

/// Search `buf[from..]` for the next Annex-B start code. Returns the
/// span of the start code itself (NOT including the NAL header byte).
pub fn find_start_code(buf: &[u8], from: usize) -> Option<StartCode> {
    if from >= buf.len() {
        return None;
    }
    let mut i = from;
    while i + 3 <= buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 {
            if buf[i + 2] == 1 {
                return Some(StartCode {
                    start: i,
                    end: i + 3,
                });
            }
            if i + 4 <= buf.len() && buf[i + 2] == 0 && buf[i + 3] == 1 {
                return Some(StartCode {
                    start: i,
                    end: i + 4,
                });
            }
        }
        i += 1;
    }
    None
}

/// Return `true` if the NAL header byte indicates an IDR slice (5),
/// SPS (7), or PPS (8). These are the access units a downstream
/// RTSP / RTP consumer treats as keyframe boundaries for seek + RTCP.
pub fn is_keyframe_unit(header_byte: u8) -> bool {
    let nal_type = header_byte & 0x1F;
    matches!(nal_type, 5 | 7 | 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_four_byte_start_code() {
        let buf = [0u8, 0, 0, 1, 0x67];
        let s = find_start_code(&buf, 0).expect("found");
        assert_eq!(s.start, 0);
        assert_eq!(s.end, 4);
    }

    #[test]
    fn finds_three_byte_start_code() {
        let buf = [0u8, 0, 1, 0x67];
        let s = find_start_code(&buf, 0).expect("found");
        assert_eq!(s.start, 0);
        assert_eq!(s.end, 3);
    }

    #[test]
    fn no_start_code_returns_none() {
        let buf = [0xff, 0x00, 0x00, 0xff];
        assert!(find_start_code(&buf, 0).is_none());
    }

    #[test]
    fn scanner_emits_two_complete_units() {
        let mut s = AnnexBScanner::default();
        // SPS, PPS — two complete units. The trailing start code on
        // `next` is what lets the scanner know the SPS is complete.
        s.push(&[
            0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1e, // SPS
            0, 0, 0, 1, 0x68, 0xce, 0x06, 0xe2, // PPS
            0, 0, 0, 1, 0x65, // IDR start; the scanner needs another
                              // start code (or EOF semantics) before
                              // it emits this one
        ]);
        let sps = s.next_unit().expect("sps");
        assert_eq!(sps, vec![0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1e]);
        let pps = s.next_unit().expect("pps");
        assert_eq!(pps, vec![0, 0, 0, 1, 0x68, 0xce, 0x06, 0xe2]);
        // The IDR unit is incomplete (no trailing start code yet) so
        // the scanner returns None until the next push.
        assert!(s.next_unit().is_none());

        s.push(&[0xab, 0xcd, 0, 0, 0, 1, 0x41]);
        let idr = s.next_unit().expect("idr");
        assert_eq!(idr, vec![0, 0, 0, 1, 0x65, 0xab, 0xcd]);
    }

    #[test]
    fn scanner_handles_split_start_code_across_pushes() {
        // Push the first three bytes of a 4-byte start code, then the
        // rest in a second push. The scanner must still find the unit.
        let mut s = AnnexBScanner::default();
        s.push(&[0, 0, 0]);
        s.push(&[1, 0x67, 0x42, 0x00, 0x1e, 0, 0, 0, 1, 0x41]);
        let unit = s.next_unit().expect("unit");
        assert_eq!(unit, vec![0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1e]);
    }

    #[test]
    fn keyframe_check_recognises_idr_sps_pps() {
        assert!(is_keyframe_unit(0x65)); // type 5 IDR
        assert!(is_keyframe_unit(0x67)); // type 7 SPS
        assert!(is_keyframe_unit(0x68)); // type 8 PPS
        assert!(!is_keyframe_unit(0x41)); // type 1 P-slice
        assert!(!is_keyframe_unit(0x06)); // type 6 SEI
    }

    #[test]
    fn contains_keyframe_walks_multiple_units() {
        // SEI, then IDR — should report true.
        let buf = [
            0, 0, 0, 1, 0x06, 0x05, 0x10, // SEI
            0, 0, 0, 1, 0x65, 0x88, 0x84, // IDR
        ];
        let mut s = AnnexBScanner::default();
        assert!(s.contains_keyframe(&buf));

        // SEI, then P-slice — should report false.
        let buf2 = [
            0, 0, 0, 1, 0x06, 0x05, 0x10, // SEI
            0, 0, 0, 1, 0x41, 0x88, 0x84, // P-slice
        ];
        let mut s2 = AnnexBScanner::default();
        assert!(!s2.contains_keyframe(&buf2));
    }
}
