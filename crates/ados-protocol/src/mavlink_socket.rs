//! The MAVLink broadcast socket wire (`/run/ados/mavlink.sock`, contract
//! `mavlink.socket`).
//!
//! Every record in both directions is a 4-byte big-endian length followed by
//! the body ([`crate::frame`]). Router → client bodies (version 2) start with
//! an 8-byte little-endian `rx_mono_ns`, the host `CLOCK_MONOTONIC` time the
//! router read that frame off the flight-controller transport (or received it
//! from any other source), followed by one raw MAVLink frame. Client → router
//! bodies are one raw MAVLink frame with no header.
//!
//! Ungated (no MAVLink dialect dependency), so a consumer that only moves raw
//! frames reads the socket without building the codec.

use std::io;

use tokio::io::AsyncReadExt;

use crate::frame::{encode_frame, FrameError, MAVLINK_MAX_FRAME};

/// Wire version of the router → client record; mirrors the `mavlink.socket`
/// row in `contracts.toml`.
pub const MAVLINK_SOCKET_VERSION: u16 = 2;

/// Length of the `rx_mono_ns` header that leads every router → client body.
pub const MAVLINK_SOCKET_HEADER_LEN: usize = 8;

/// Largest router → client body: the frame cap plus the header.
pub const MAVLINK_SOCKET_MAX_BODY: usize = MAVLINK_MAX_FRAME + MAVLINK_SOCKET_HEADER_LEN;

/// One frame as a client receives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketFrame {
    /// Host monotonic ns at which the router received the frame.
    pub rx_mono_ns: u64,
    /// The raw MAVLink frame, byte for byte.
    pub frame: Vec<u8>,
}

/// The router → client body: header then frame (no length prefix).
pub fn encode_body(rx_mono_ns: u64, frame: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(MAVLINK_SOCKET_HEADER_LEN + frame.len());
    body.extend_from_slice(&rx_mono_ns.to_le_bytes());
    body.extend_from_slice(frame);
    body
}

/// The complete router → client record: length prefix, header, frame.
pub fn encode_record(rx_mono_ns: u64, frame: &[u8]) -> Result<Vec<u8>, FrameError> {
    encode_frame(&encode_body(rx_mono_ns, frame), MAVLINK_SOCKET_MAX_BODY)
}

/// Split a router → client body into `(rx_mono_ns, frame)`. `None` when the
/// body is too short to carry the header.
pub fn decode_body(body: &[u8]) -> Option<(u64, &[u8])> {
    let (head, frame) = body.split_at_checked(MAVLINK_SOCKET_HEADER_LEN)?;
    let ns = u64::from_le_bytes(head.try_into().ok()?);
    Some((ns, frame))
}

/// Read one router → client record. `Ok(None)` on clean EOF; a body shorter
/// than the header is `InvalidData`.
pub async fn read_frame<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> io::Result<Option<SocketFrame>> {
    let Some(mut body) =
        crate::ipc::read_length_prefixed(reader, MAVLINK_SOCKET_MAX_BODY, false).await?
    else {
        return Ok(None);
    };
    let Some((rx_mono_ns, _)) = decode_body(&body) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "mavlink socket record shorter than its header",
        ));
    };
    body.drain(..MAVLINK_SOCKET_HEADER_LEN);
    Ok(Some(SocketFrame {
        rx_mono_ns,
        frame: body,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_round_trips() {
        let frame = [0xFD, 9, 0, 0, 5, 1, 1, 0, 0, 0];
        let body = encode_body(0x0102_0304_0506_0708, &frame);
        assert_eq!(&body[..8], &[8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(
            decode_body(&body),
            Some((0x0102_0304_0506_0708, &frame[..]))
        );
        assert_eq!(decode_body(&body[..7]), None);
        assert_eq!(
            decode_body(&body[..8]),
            Some((0x0102_0304_0506_0708, &[][..]))
        );
    }

    #[tokio::test]
    async fn record_reads_back_through_the_length_prefix() {
        let mut wire = encode_record(42, b"\xfeframe-a").unwrap();
        assert_eq!(&wire[..4], &(8u32 + 8).to_be_bytes());
        wire.extend(encode_record(43, b"\xfdframe-bb").unwrap());
        let mut r = &wire[..];
        let a = read_frame(&mut r).await.unwrap().unwrap();
        assert_eq!(a.rx_mono_ns, 42);
        assert_eq!(a.frame, b"\xfeframe-a");
        let b = read_frame(&mut r).await.unwrap().unwrap();
        assert_eq!(
            (b.rx_mono_ns, b.frame.as_slice()),
            (43, &b"\xfdframe-bb"[..])
        );
        assert_eq!(read_frame(&mut r).await.unwrap(), None);
    }

    #[tokio::test]
    async fn short_body_is_invalid_data() {
        let wire = encode_frame(&[1, 2, 3], MAVLINK_SOCKET_MAX_BODY).unwrap();
        let err = read_frame(&mut &wire[..]).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
