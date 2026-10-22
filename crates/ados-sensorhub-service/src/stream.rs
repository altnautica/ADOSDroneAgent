//! The receive side of the hub link: splits the byte stream on the `0x00`
//! delimiter, COBS-decodes and CRC-checks each frame, and hands every good
//! packet to the caller. Bad frames are counted and dropped; the next
//! delimiter resynchronises the stream.

use ados_sensorhub::{self as proto, Deframer, Packet, Push};

/// Integrity counters of one decoder.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DecodeStats {
    /// Frames whose CRC did not match.
    pub crc_errors: u64,
    /// Frames rejected before the CRC check: malformed COBS, a length that
    /// disagrees with the frame, or a frame longer than the protocol allows.
    pub framing_errors: u64,
    /// CRC-good frames whose payload does not fit their type.
    pub bad_payloads: u64,
    /// CRC-good frames of a type this build does not know (skipped).
    pub unknown: u64,
    /// Packets delivered.
    pub packets: u64,
}

impl DecodeStats {
    /// Frames lost to corruption on the link (CRC or framing).
    pub fn corrupt(&self) -> u64 {
        self.crc_errors + self.framing_errors
    }
}

/// Incremental stream decoder; feed it reads of any size.
pub struct StreamDecoder {
    deframer: Deframer<{ proto::MAX_FRAME }>,
    raw: [u8; proto::MAX_RAW],
    stats: DecodeStats,
}

impl Default for StreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamDecoder {
    pub fn new() -> Self {
        Self {
            deframer: Deframer::new(),
            raw: [0; proto::MAX_RAW],
            stats: DecodeStats::default(),
        }
    }

    pub fn stats(&self) -> DecodeStats {
        self.stats
    }

    /// Push `bytes`, calling `on_packet` for every complete good packet.
    pub fn push(&mut self, bytes: &[u8], mut on_packet: impl FnMut(Packet<'_>)) {
        for &b in bytes {
            match self.deframer.push(b) {
                Push::Pending => {}
                Push::Overflow => self.stats.framing_errors += 1,
                Push::Block(block) => match proto::decode_frame(block, &mut self.raw) {
                    Ok(Packet::Unknown { .. }) => self.stats.unknown += 1,
                    Ok(pkt) => {
                        self.stats.packets += 1;
                        on_packet(pkt);
                    }
                    Err(proto::Error::Crc) => self.stats.crc_errors += 1,
                    Err(proto::Error::PayloadSize { .. } | proto::Error::ImuCount) => {
                        self.stats.bad_payloads += 1
                    }
                    Err(_) => self.stats.framing_errors += 1,
                },
            }
        }
    }
}

/// Encode one packet as a wire frame (COBS + delimiter).
pub fn encode(pkt: &Packet<'_>) -> Result<Vec<u8>, proto::Error> {
    let mut scratch = [0u8; proto::MAX_RAW];
    let mut out = vec![0u8; proto::MAX_FRAME];
    let n = proto::encode_frame(pkt, &mut scratch, &mut out)?;
    out.truncate(n);
    Ok(out)
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
