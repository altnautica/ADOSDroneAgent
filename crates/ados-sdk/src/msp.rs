//! Streaming MSP frame reassembly for `ctx.msp.subscribe`.
//!
//! The host delivers the FC->plugin MSP byte stream in whatever chunks the
//! serial read produced — a chunk is NOT a frame. A plugin that calls
//! [`ados_protocol::msp::decode_frame`] once per delivered chunk drops every
//! frame that straddles two chunks (and mis-reads one that arrives glued to the
//! next). This buffers across deliveries and yields whole frames only, the way a
//! real MSP host reads its port.
//!
//! Feed each `msp.deliver` chunk to [`MspReassembler::push`]; it returns every
//! complete frame now available and retains the trailing partial. The CRC is
//! verified by `decode_frame`, so a corrupt frame never surfaces as valid; on a
//! bad preamble or CRC the reader resyncs to the next `$` rather than wedging.

use ados_protocol::msp::{decode_frame, MspDecodeError, MspFrame};

/// A real MSP frame cannot exceed the v2 header + a `u16` payload + CRC. If the
/// buffer grows past this without yielding a frame the stream has desynced (a
/// misread size field), so the reader resyncs instead of waiting forever for
/// bytes that will never complete a bogus frame.
const MAX_MSP_FRAME: usize = 9 + u16::MAX as usize + 1;

/// Accumulates the delivered MSP byte stream and emits whole frames.
#[derive(Debug, Default)]
pub struct MspReassembler {
    buf: Vec<u8>,
}

impl MspReassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a delivered chunk and return every complete frame now decodable.
    /// The trailing partial frame is retained for the next call. Frames whose CRC
    /// fails are skipped (never returned as valid); the reader resyncs to the
    /// next preamble.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<MspFrame> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            match decode_frame(&self.buf) {
                Ok((frame, consumed)) => {
                    out.push(frame);
                    self.buf.drain(..consumed);
                }
                // A valid preamble but not all the bytes yet: wait for more —
                // unless the buffer has grown past any real frame, which means we
                // are locked onto a bogus size field and must resync.
                Err(MspDecodeError::TooShort) => {
                    if self.buf.len() > MAX_MSP_FRAME {
                        self.resync();
                        continue;
                    }
                    break;
                }
                // Not aligned on a frame, or a complete-but-corrupt frame: drop to
                // the next `$` and retry. If there is none, the buffer is cleared.
                Err(MspDecodeError::BadPreamble) | Err(MspDecodeError::BadCrc { .. }) => {
                    self.resync();
                }
            }
        }
        out
    }

    /// The bytes buffered but not yet a complete frame (mostly for tests).
    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    /// Drop everything up to and including the current leading byte, advancing to
    /// the next `$` in the buffer; clear it if there is none. Always shrinks the
    /// buffer, so [`Self::push`]'s loop cannot spin.
    fn resync(&mut self) {
        match self.buf.iter().skip(1).position(|&b| b == b'$') {
            Some(rel) => {
                self.buf.drain(..rel + 1);
            }
            None => self.buf.clear(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::msp::set_raw_rc;

    /// A `$M<`/`$X<`-framed request the FC would echo; `set_raw_rc` gives a real
    /// v2 frame with a correct CRC to split.
    fn frame() -> Vec<u8> {
        set_raw_rc(&[1500, 1500, 1000, 1500], true).unwrap()
    }

    #[test]
    fn a_frame_split_across_two_chunks_decodes() {
        // The whole point: a single decode_frame per delivered chunk would drop
        // this frame because neither half is complete on its own.
        let f = frame();
        let mid = f.len() / 2;
        let mut r = MspReassembler::new();
        assert!(r.push(&f[..mid]).is_empty(), "no frame from the first half");
        assert_eq!(r.pending(), mid);
        let got = r.push(&f[mid..]);
        assert_eq!(got.len(), 1, "the reassembled frame appears on the second half");
        assert_eq!(got[0].cmd, 200); // MSP_SET_RAW_RC
    }

    #[test]
    fn two_frames_glued_in_one_chunk_both_decode() {
        let mut glued = frame();
        glued.extend_from_slice(&frame());
        let got = MspReassembler::new().push(&glued);
        assert_eq!(got.len(), 2, "both glued frames decode from one delivery");
    }

    #[test]
    fn leading_garbage_is_resynced_past() {
        let f = frame();
        let mut chunk = vec![0xde, 0xad, 0xbe];
        chunk.extend_from_slice(&f);
        let got = MspReassembler::new().push(&chunk);
        assert_eq!(got.len(), 1, "the reader resyncs to the real preamble");
    }

    #[test]
    fn a_corrupt_frame_is_skipped_not_yielded() {
        // Flip a payload byte so the CRC fails; the reader must resync and decode
        // the following good frame rather than surface the corrupt one.
        let mut corrupt = frame();
        let mid = corrupt.len() / 2;
        corrupt[mid] ^= 0xff;
        let mut stream = corrupt;
        stream.extend_from_slice(&frame());
        let got = MspReassembler::new().push(&stream);
        assert_eq!(
            got.len(),
            1,
            "only the good frame surfaces; the corrupt one is dropped, not returned"
        );
    }

    #[test]
    fn a_desynced_stream_does_not_grow_without_bound() {
        // No preamble ever: the buffer must not accumulate forever.
        let mut r = MspReassembler::new();
        for _ in 0..1000 {
            assert!(r.push(&[0u8; 256]).is_empty());
        }
        assert!(r.pending() <= 256, "a preamble-less stream is drained, not hoarded");
    }
}
