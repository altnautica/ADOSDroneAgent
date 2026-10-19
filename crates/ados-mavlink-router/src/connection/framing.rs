//! MAVLink byte-stream framing.
//!
//! Splits the inbound serial/network byte stream into complete MAVLink frames
//! (v1 `0xFE` and v2 `0xFD`), tolerating junk before the next start-of-frame
//! magic and leaving any partial trailing frame buffered for the next read.
//! A candidate whose message id is in the dialect has its checksum verified:
//! one that fails is not a frame, and the search resumes one byte after its
//! start-of-frame, so a dropped byte costs the damaged frame and nothing after
//! it. A candidate whose id is outside the dialect has no known CRC_EXTRA, so
//! its checksum cannot be checked; it is framed by its header length and
//! forwarded verbatim (custom and newer-dialect messages must pass through).

use ados_protocol::mavlink::{MavMessage, Message};
use bytes::Bytes;

/// Both MAVLink start-of-frame magic bytes: `0xFD` (v2) and `0xFE` (v1).
pub(crate) const STX_V2: u8 = 0xFD;
pub(crate) const STX_V1: u8 = 0xFE;

/// Count MSP frame-start sequences in a raw byte slice: MSPv1 `$M<` / `$M>`
/// (`24 4D 3C/3E`) and MSPv2 `$X<` / `$X>` (`24 58 3C/3E`).
///
/// Used to detect a flight controller that is emitting MSP instead of MAVLink on
/// its serial port (e.g. a board whose USB port is left on MSP rather than
/// MAVLink), so the agent can tell the operator the link speaks the wrong
/// protocol rather than silently reporting no flight controller. This is
/// observe-only: MSP frames are never decoded, acted on, or forwarded to the
/// flight controller — only the three-byte frame-start signatures are counted.
pub(crate) fn count_msp_frame_starts(bytes: &[u8]) -> usize {
    bytes
        .windows(3)
        .filter(|w| {
            w[0] == b'$' && (w[1] == b'M' || w[1] == b'X') && (w[2] == b'<' || w[2] == b'>')
        })
        .count()
}

/// Total byte length of a complete frame whose head is at `buf[0]`, or `None`
/// when more bytes are needed (or the head is not a recognised magic byte).
///
/// A v2 frame is `0xFD`, a 1-byte payload length `L`, the rest of the 10-byte
/// header, `L` payload bytes, a 2-byte checksum, and (when the incompat-flags
/// signed bit is set) a 13-byte signature. A v1 frame is `0xFE`, a 1-byte
/// payload length `L`, a 6-byte header total, `L` payload bytes, and a 2-byte
/// checksum (no incompat/compat flags, no signature).
pub(crate) fn frame_total_len(buf: &[u8]) -> Option<usize> {
    match buf.first().copied() {
        Some(STX_V2) => {
            // Need the length and incompat-flags bytes to size a v2 frame.
            if buf.len() < 3 {
                return None;
            }
            let payload_len = buf[1] as usize;
            let signed = (buf[2] & 0x01) != 0;
            Some(10 + payload_len + 2 + if signed { 13 } else { 0 })
        }
        Some(STX_V1) => {
            // Need the length byte to size a v1 frame.
            if buf.len() < 2 {
                return None;
            }
            let payload_len = buf[1] as usize;
            Some(6 + payload_len + 2)
        }
        _ => None,
    }
}

/// Message id carried in the header of a candidate frame at `buf[0]`, or
/// `None` when the header is not complete yet.
pub(super) fn frame_msg_id(buf: &[u8]) -> Option<u32> {
    match buf.first().copied() {
        Some(STX_V2) if buf.len() >= 10 => Some(u32::from_le_bytes([buf[7], buf[8], buf[9], 0])),
        Some(STX_V1) if buf.len() >= 6 => Some(u32::from(buf[5])),
        _ => None,
    }
}

/// Whether the dialect knows `msg_id`, i.e. its CRC_EXTRA is available and the
/// frame checksum can be verified.
pub(super) fn msg_id_known(msg_id: u32) -> bool {
    <MavMessage as Message>::default_message_from_id(msg_id).is_some()
}

/// Drain every complete MAVLink frame (v1 `0xFE` and v2 `0xFD`) from the head
/// of `buf`, returning the raw frames and leaving any partial trailing frame in
/// `buf`. Junk before the next magic byte is dropped. A known-id candidate
/// whose checksum fails is skipped one byte at a time, so a start-of-frame byte
/// inside a payload (after a UART overrun dropped a byte) cannot swallow the
/// real frames that follow. An unknown-id candidate is forwarded as framed by
/// its header length, since its checksum is unverifiable. Returns when the
/// buffer holds only a partial frame.
///
/// Yields [`Bytes`] rather than `Vec<u8>` because each frame is immediately
/// handed to every fan-out consumer — the IPC socket, the aux tee, the
/// relayed-vehicle projection, and one task per connected GCS proxy client.
/// Copying the frame out of the reassembly buffer once is unavoidable; copying
/// it again per consumer is not, and that second cost scaled with the number of
/// connected clients. The bytes themselves are unchanged: the frame is still
/// forwarded verbatim, which the IPC frame contract requires.
///
/// The walk advances a cursor and drains the consumed prefix once at the end,
/// so a read carrying many frames costs one memmove rather than one per frame.
pub(crate) fn extract_frames(buf: &mut Vec<u8>) -> Vec<Bytes> {
    let mut out = Vec::new();
    let mut start = 0usize;
    loop {
        // Skip bytes before the next start-of-frame magic (either version).
        match buf[start..]
            .iter()
            .position(|&b| b == STX_V2 || b == STX_V1)
        {
            Some(n) => start += n,
            None => {
                start = buf.len();
                break;
            }
        }
        let rest = &buf[start..];
        let Some(total) = frame_total_len(rest) else {
            // Too few bytes to size the frame yet. Wait for more bytes.
            break;
        };
        if rest.len() < total {
            break;
        }
        let candidate = &rest[..total];
        // `total` covers the full header, so the id is always present here.
        let known = frame_msg_id(candidate).is_some_and(msg_id_known);
        if known && !ados_protocol::mavlink::frame_checksum_ok(candidate) {
            // Not a frame: resume the search just past this start byte.
            start += 1;
            continue;
        }
        out.push(Bytes::copy_from_slice(candidate));
        start += total;
    }
    buf.drain(..start);
    out
}

#[cfg(test)]
mod tests;
