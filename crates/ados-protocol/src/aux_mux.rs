//! Auxiliary-lane multiplex framing.
//!
//! The WFB radio carries three planes: video on radio id 0, tiny control frames
//! on radio id 1, and a general-purpose application lane on radio id 2. The
//! application lane is a single UDP-ish datagram pipe, so several logical
//! streams sharing it need a way to say which stream a datagram belongs to.
//! This module is that framing, and it is the ONLY place either rig decides
//! what an aux datagram means.
//!
//! ## Why not reuse an existing envelope
//!
//! [`crate::tunnel_config`] chunks a body across 128-byte MAVLink TUNNEL frames
//! and is sized for that budget. The aux lane is not so constrained: a decoded
//! aux datagram carries a normal payload, so it needs a channel tag and a
//! length, not a chunking protocol. The plugin msgpack envelope is far heavier
//! than a per-frame header should be on a lane that also carries MAVLink at
//! telemetry rates.
//!
//! ## Wire layout
//!
//! ```text
//!   byte 0..2   magic   = [0xAD, 0x02]   (0xAD marks an ADOS frame, 0x02 the
//!                                         aux lane; distinguishes our frames
//!                                         from anything else sharing radio 2)
//!   byte 2      version = 0x01
//!   byte 3      channel                  (see [`AuxChannel`])
//!   byte 4..6   len     (u16, big-endian) — payload byte count
//!   byte 6..    payload
//! ```
//!
//! Six bytes of overhead per datagram. Big-endian length matches the rest of
//! the agent's framed IPC (the 4-byte-BE MAVLink socket framing), so a reader
//! written against one does not silently misparse the other.
//!
//! A frame whose magic or version does not match is DROPPED, never guessed at.
//! The lane is shared and an unrecognised frame is far more likely to be
//! another application's traffic than a frame of ours worth salvaging.

/// Fixed header size in bytes.
pub const AUX_HEADER_LEN: usize = 6;

/// Frame magic: an ADOS frame (0xAD) on the aux lane (0x02).
pub const AUX_MAGIC: [u8; 2] = [0xAD, 0x02];

/// The framing version this build speaks.
pub const AUX_VERSION: u8 = 0x01;

/// Largest payload one aux frame may carry.
///
/// The radio hands the aux receiver whole decoded datagrams. Keeping a frame
/// under a typical MTU avoids relying on IP fragmentation over the loopback
/// hand-off, and bounds the reassembly buffer a reader must hold.
pub const AUX_MAX_PAYLOAD: usize = 1200;

/// Which logical stream an aux frame belongs to.
///
/// Values are explicit and MUST NOT be renumbered: both rigs may run different
/// agent builds across an upgrade, and a renumber would silently reroute one
/// stream's frames into another stream's parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AuxChannel {
    /// MAVLink frames from the drone's flight controller, so a ground station
    /// can republish them and drive the vehicle exactly as a direct link would.
    Mavlink = 1,
    /// Periodic compact node-status snapshots (services, capabilities, health).
    Status = 2,
    /// Node identity and capability advertisement.
    Identity = 3,
    /// Relay-proxy HTTP request, ground → drone over the uplink (radio_id 3).
    /// A ground station paired to a drone only over WFB has no IP reach to the
    /// linked drone, so a proxy request rides the aux uplink instead. The
    /// drone's `aux_uplink_consumer` decodes and dispatches against its own
    /// HTTP API, then radiates a [`AuxChannel::Response`] back.
    Request = 4,
    /// Relay-proxy HTTP response, drone → ground over the downlink (radio_id
    /// 2). Carries the same request id the matching `Request` was sent with.
    Response = 5,
}

impl AuxChannel {
    /// Parse a channel byte. Unknown channels return `None` so a reader drops
    /// the frame instead of feeding it to the wrong parser.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Mavlink),
            2 => Some(Self::Status),
            3 => Some(Self::Identity),
            4 => Some(Self::Request),
            5 => Some(Self::Response),
            _ => None,
        }
    }
}

/// Why a frame could not be decoded. Distinct variants so a caller can count
/// them separately: foreign traffic on a shared lane is normal, whereas a
/// truncated frame of ours points at a real transport fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuxDecodeError {
    /// Shorter than the fixed header.
    TooShort,
    /// Magic did not match: almost certainly not our frame.
    BadMagic,
    /// A version this build does not speak.
    UnsupportedVersion(u8),
    /// A channel this build does not know.
    UnknownChannel(u8),
    /// The declared length does not match the bytes actually present.
    LengthMismatch { declared: usize, actual: usize },
    /// The declared length exceeds [`AUX_MAX_PAYLOAD`].
    TooLong(usize),
}

/// Encode one payload as an aux frame.
///
/// Returns `None` when the payload exceeds [`AUX_MAX_PAYLOAD`]; callers split
/// or drop rather than silently truncating, because a truncated MAVLink frame
/// would fail CRC on the far side and look like radio corruption.
pub fn encode(channel: AuxChannel, payload: &[u8]) -> Option<Vec<u8>> {
    if payload.len() > AUX_MAX_PAYLOAD {
        return None;
    }
    let mut out = Vec::with_capacity(AUX_HEADER_LEN + payload.len());
    out.extend_from_slice(&AUX_MAGIC);
    out.push(AUX_VERSION);
    out.push(channel as u8);
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(payload);
    Some(out)
}

/// Decode one aux frame into its channel and payload slice.
pub fn decode(frame: &[u8]) -> Result<(AuxChannel, &[u8]), AuxDecodeError> {
    if frame.len() < AUX_HEADER_LEN {
        return Err(AuxDecodeError::TooShort);
    }
    if frame[0..2] != AUX_MAGIC {
        return Err(AuxDecodeError::BadMagic);
    }
    if frame[2] != AUX_VERSION {
        return Err(AuxDecodeError::UnsupportedVersion(frame[2]));
    }
    let channel = AuxChannel::from_u8(frame[3]).ok_or(AuxDecodeError::UnknownChannel(frame[3]))?;
    let declared = u16::from_be_bytes([frame[4], frame[5]]) as usize;
    if declared > AUX_MAX_PAYLOAD {
        return Err(AuxDecodeError::TooLong(declared));
    }
    let actual = frame.len() - AUX_HEADER_LEN;
    if declared != actual {
        return Err(AuxDecodeError::LengthMismatch { declared, actual });
    }
    Ok((channel, &frame[AUX_HEADER_LEN..]))
}

// MAVLink frame boundaries live here rather than in the dialect-backed
// `mavlink` module because splitting a batch needs only the header, and the
// ground station must not pull an entire dialect crate in to receive frames
// it merely forwards.

/// Total on-wire byte length of the MAVLink frame starting at `bytes[0]`, or
/// `None` when the slice does not yet hold a whole frame.
///
/// Reads only the header, never the dialect, so a message id this build does
/// not know still yields a correct boundary. That matters wherever frames are
/// concatenated: a dialect-dependent split would desynchronise the whole batch
/// on the first unknown message.
///
/// v2: 10 header bytes + payload + 2 checksum, plus a 13-byte signature when
/// the incompat flag bit 0 is set. v1: 6 header bytes + payload + 2 checksum.
pub fn frame_len(bytes: &[u8]) -> Option<usize> {
    match bytes.first()? {
        0xFD => {
            let payload = *bytes.get(1)? as usize;
            let signed = (*bytes.get(2)? & 0x01) != 0;
            Some(12 + payload + if signed { 13 } else { 0 })
        }
        0xFE => Some(8 + *bytes.get(1)? as usize),
        _ => None,
    }
}

/// Split a buffer of back-to-back MAVLink frames into whole frames.
///
/// Several frames are batched into one radio datagram, because the auxiliary
/// lane's loss is driven by packets per second rather than bytes per second:
/// measured on a live link, the same byte rate lost 15% as many small datagrams
/// and 0% as a few large ones. Batching preserves the frames that a
/// packet-rate cap would otherwise shed.
///
/// Stops at the first byte that does not begin a complete, recognisable frame
/// and returns what was whole, so a truncated tail is dropped rather than
/// mis-split into garbage.
pub fn split_frames(buf: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < buf.len() {
        let Some(len) = frame_len(&buf[at..]) else {
            break;
        };
        if len == 0 || at + len > buf.len() {
            break;
        }
        out.push(&buf[at..at + len]);
        at += len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_channel() {
        for ch in [
            AuxChannel::Mavlink,
            AuxChannel::Status,
            AuxChannel::Identity,
            AuxChannel::Request,
            AuxChannel::Response,
        ] {
            let body = b"payload bytes";
            let framed = encode(ch, body).expect("within budget");
            assert_eq!(framed.len(), AUX_HEADER_LEN + body.len());
            let (got_ch, got) = decode(&framed).expect("decodes");
            assert_eq!(got_ch, ch);
            assert_eq!(got, body);
        }
    }

    #[test]
    fn round_trips_an_empty_payload() {
        let framed = encode(AuxChannel::Status, &[]).unwrap();
        let (ch, body) = decode(&framed).unwrap();
        assert_eq!(ch, AuxChannel::Status);
        assert!(body.is_empty());
    }

    #[test]
    fn refuses_to_truncate_an_oversized_payload() {
        // Silently truncating would produce a MAVLink frame that fails CRC on
        // the far side and reads as radio corruption rather than our bug.
        let too_big = vec![0u8; AUX_MAX_PAYLOAD + 1];
        assert!(encode(AuxChannel::Mavlink, &too_big).is_none());
        assert!(encode(AuxChannel::Mavlink, &vec![0u8; AUX_MAX_PAYLOAD]).is_some());
    }

    #[test]
    fn drops_foreign_traffic_rather_than_guessing() {
        assert_eq!(decode(b"hello!").unwrap_err(), AuxDecodeError::BadMagic);
        assert_eq!(decode(b"\xAD").unwrap_err(), AuxDecodeError::TooShort);
    }

    #[test]
    fn rejects_an_unsupported_version_and_unknown_channel() {
        let mut f = encode(AuxChannel::Mavlink, b"x").unwrap();
        f[2] = 0x99;
        assert_eq!(
            decode(&f).unwrap_err(),
            AuxDecodeError::UnsupportedVersion(0x99)
        );

        let mut f = encode(AuxChannel::Mavlink, b"x").unwrap();
        f[3] = 0x7F;
        assert_eq!(
            decode(&f).unwrap_err(),
            AuxDecodeError::UnknownChannel(0x7F)
        );
    }

    #[test]
    fn catches_a_truncated_frame_of_ours() {
        // Distinct from BadMagic: this is our frame arriving damaged, which
        // points at a transport fault rather than another application's traffic.
        let mut f = encode(AuxChannel::Mavlink, b"twelve bytes").unwrap();
        f.truncate(f.len() - 3);
        assert_eq!(
            decode(&f).unwrap_err(),
            AuxDecodeError::LengthMismatch {
                declared: 12,
                actual: 9
            }
        );
    }

    /// Build a minimal well-formed MAVLink v2 frame with `payload` bytes.
    fn v2(payload_len: u8, signed: bool) -> Vec<u8> {
        let mut f = vec![
            0xFD,
            payload_len,
            if signed { 0x01 } else { 0x00 },
            0,
            0,
            1,
            1,
            0,
            0,
            0,
        ];
        f.extend(std::iter::repeat_n(0xAB, payload_len as usize));
        f.extend_from_slice(&[0, 0]); // checksum
        if signed {
            f.extend(std::iter::repeat_n(0u8, 13));
        }
        f
    }

    /// Build a minimal well-formed MAVLink v1 frame.
    fn v1(payload_len: u8) -> Vec<u8> {
        let mut f = vec![0xFE, payload_len, 0, 1, 1, 0];
        f.extend(std::iter::repeat_n(0xCD, payload_len as usize));
        f.extend_from_slice(&[0, 0]);
        f
    }

    #[test]
    fn frame_len_reads_v1_v2_and_the_signature_block() {
        assert_eq!(frame_len(&v2(10, false)), Some(22));
        assert_eq!(frame_len(&v2(10, true)), Some(35));
        assert_eq!(frame_len(&v1(10)), Some(18));
        assert_eq!(frame_len(b"\x00junk"), None);
        assert_eq!(frame_len(&[]), None);
    }

    #[test]
    fn splits_a_batch_back_into_the_frames_that_went_in() {
        // The whole point of batching: the lane loses packets, not bytes, so
        // several frames ride one datagram. Split must return exactly what was
        // concatenated or the republished stream is corrupt.
        let frames = vec![v2(5, false), v1(9), v2(0, false), v2(3, true)];
        let mut batch = Vec::new();
        for f in &frames {
            batch.extend_from_slice(f);
        }
        let out = split_frames(&batch);
        assert_eq!(out.len(), frames.len());
        for (got, want) in out.iter().zip(frames.iter()) {
            assert_eq!(*got, want.as_slice());
        }
    }

    #[test]
    fn a_single_frame_batch_is_unchanged() {
        let f = v2(12, false);
        assert_eq!(split_frames(&f), vec![f.as_slice()]);
    }

    #[test]
    fn drops_a_truncated_tail_and_keeps_the_whole_frames() {
        // A datagram clipped in transit must not be mis-split into garbage: the
        // frames that arrived whole are kept, the partial one is discarded.
        let good = v2(8, false);
        let mut batch = good.clone();
        let partial = v2(20, false);
        batch.extend_from_slice(&partial[..6]);
        let out = split_frames(&batch);
        assert_eq!(out, vec![good.as_slice()]);
    }

    #[test]
    fn stops_at_an_unrecognisable_byte_rather_than_scanning_on() {
        let mut batch = v2(4, false);
        batch.push(0x00);
        batch.extend_from_slice(&v1(4));
        // Everything after the junk byte is abandoned: resyncing by scanning
        // could lock onto a payload byte that happens to look like a header.
        assert_eq!(split_frames(&batch).len(), 1);
    }

    #[test]
    fn an_empty_buffer_yields_no_frames() {
        assert!(split_frames(&[]).is_empty());
    }

    #[test]
    fn channel_numbers_are_pinned() {
        // Both rigs may run different builds across an upgrade. Renumbering
        // would route one stream's frames into another stream's parser.
        assert_eq!(AuxChannel::Mavlink as u8, 1);
        assert_eq!(AuxChannel::Status as u8, 2);
        assert_eq!(AuxChannel::Identity as u8, 3);
        assert_eq!(AuxChannel::Request as u8, 4);
        assert_eq!(AuxChannel::Response as u8, 5);
        assert_eq!(AuxChannel::from_u8(1), Some(AuxChannel::Mavlink));
        assert_eq!(AuxChannel::from_u8(0), None);
    }
}
