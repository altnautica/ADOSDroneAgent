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
}

impl AuxChannel {
    /// Parse a channel byte. Unknown channels return `None` so a reader drops
    /// the frame instead of feeding it to the wrong parser.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Mavlink),
            2 => Some(Self::Status),
            3 => Some(Self::Identity),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_channel() {
        for ch in [
            AuxChannel::Mavlink,
            AuxChannel::Status,
            AuxChannel::Identity,
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

    #[test]
    fn channel_numbers_are_pinned() {
        // Both rigs may run different builds across an upgrade. Renumbering
        // would route one stream's frames into another stream's parser.
        assert_eq!(AuxChannel::Mavlink as u8, 1);
        assert_eq!(AuxChannel::Status as u8, 2);
        assert_eq!(AuxChannel::Identity as u8, 3);
        assert_eq!(AuxChannel::from_u8(1), Some(AuxChannel::Mavlink));
        assert_eq!(AuxChannel::from_u8(0), None);
    }
}
