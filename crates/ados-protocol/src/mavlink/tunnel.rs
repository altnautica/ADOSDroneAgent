//! MAVLink TUNNEL frame building and parsing.

use super::*;

/// MAVLink message id of `TUNNEL`.
pub const MSG_ID_TUNNEL: u32 = 385;

/// The `TUNNEL` CRC_EXTRA, the message-definition seed the X.25 checksum folds in
/// last (the value the canonical dialect carries for the message; not the id).
pub const TUNNEL_CRC_EXTRA: u8 = 147;

/// Maximum bytes a single TUNNEL frame can carry: the fixed-size payload array.
pub const TUNNEL_MAX_PAYLOAD: usize = 128;

/// The highest `payload_type` value reserved by the standard registry. Values at
/// or below this are reserved; an application uses a value strictly above it so
/// it can never collide with a registered type, and so an unrelated peer that
/// does not recognize the value ignores the frame. The numeric field on the wire
/// is a u16, so the usable application range is `32768..=65535`.
pub const TUNNEL_RESERVED_PAYLOAD_TYPE_MAX: u16 = 32767;

/// A TUNNEL build/parse failure with a stable, human-readable message (not localized).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelError(pub String);

impl std::fmt::Display for TunnelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TunnelError {}

/// Build a complete MAVLink v2 `TUNNEL` frame carrying an opaque application
/// `payload` tagged with a private `payload_type`, returning the raw frame bytes
/// ready to write to the MAVLink socket.
///
/// The generated dialect models `payload_type` as a named enum, so a private
/// (unregistered) value cannot ride the typed `MavMessage::TUNNEL` path — the
/// same constraint [`build_command_long_v2`] works around for an unnamed command
/// id. This serializes the wire frame directly: the `TUNNEL` payload in field
/// order (`payload_type` u16-LE, `target_system` u8, `target_component` u8,
/// `payload_length` u8, then the 128-byte payload, the unused tail zero-padded),
/// MAVLink2 trailing-zero truncation, the v2 header, and the X.25 checksum folded
/// with [`TUNNEL_CRC_EXTRA`]. The result is byte-identical to a `TUNNEL` the
/// typed serializer would emit for the same values; the only difference is this
/// accepts a `payload_type` the enum has no variant for.
///
/// The frame is unsigned (no MAVLink2 signature; incompat/compat flags = 0).
/// The tunnel is a transparent opaque pipe: this owns no application semantics,
/// so any per-payload integrity (an HMAC, a replay counter) lives inside
/// `payload`, not here.
///
/// Rejects a `payload_type` at or below [`TUNNEL_RESERVED_PAYLOAD_TYPE_MAX`] (a
/// registered/reserved value, never to be minted by an application) and a
/// `payload` longer than [`TUNNEL_MAX_PAYLOAD`].
pub fn build_tunnel_v2(
    header: MavHeader,
    payload_type: u16,
    target_system: u8,
    target_component: u8,
    payload: &[u8],
) -> Result<Vec<u8>, TunnelError> {
    if payload_type <= TUNNEL_RESERVED_PAYLOAD_TYPE_MAX {
        return Err(TunnelError(format!(
            "payload_type {payload_type} must be a private type greater than {TUNNEL_RESERVED_PAYLOAD_TYPE_MAX}"
        )));
    }
    if payload.len() > TUNNEL_MAX_PAYLOAD {
        return Err(TunnelError(format!(
            "payload is {} bytes, exceeds the {TUNNEL_MAX_PAYLOAD}-byte TUNNEL limit",
            payload.len()
        )));
    }

    // The TUNNEL payload in wire (field-id) order: payload_type (u16-LE),
    // target_system, target_component, payload_length, then the fixed 128-byte
    // payload array (the unused tail stays zero). payload_length records the
    // application's used count; the array itself is always the full width before
    // MAVLink2 trailing-zero truncation.
    let mut body = Vec::with_capacity(5 + TUNNEL_MAX_PAYLOAD);
    body.extend_from_slice(&payload_type.to_le_bytes());
    body.push(target_system);
    body.push(target_component);
    body.push(payload.len() as u8);
    body.extend_from_slice(payload);
    // Zero-pad the fixed-width payload array out to its full size.
    body.resize(5 + TUNNEL_MAX_PAYLOAD, 0);

    // MAVLink2 truncates trailing zero bytes off the payload, keeping at least
    // one byte; the CRC is computed over the truncated payload.
    truncate_trailing_zeros(&mut body);

    let mut frame = Vec::with_capacity(10 + body.len() + 2);
    frame.push(0xFD); // v2 start-of-frame
    frame.push(body.len() as u8); // payload length
    frame.push(0x00); // incompat flags (unsigned)
    frame.push(0x00); // compat flags
    frame.push(header.sequence);
    frame.push(header.system_id);
    frame.push(header.component_id);
    // 3-byte little-endian message id.
    frame.push((MSG_ID_TUNNEL & 0xFF) as u8);
    frame.push(((MSG_ID_TUNNEL >> 8) & 0xFF) as u8);
    frame.push(((MSG_ID_TUNNEL >> 16) & 0xFF) as u8);
    frame.extend_from_slice(&body);

    // X.25 checksum over every byte after the start-of-frame, then the CRC_EXTRA.
    let mut crc = X25_INIT;
    for &b in &frame[1..] {
        crc = x25_accumulate(b, crc);
    }
    crc = x25_accumulate(TUNNEL_CRC_EXTRA, crc);
    frame.push((crc & 0xFF) as u8);
    frame.push((crc >> 8) as u8);

    Ok(frame)
}

/// The `payload_type` of a raw MAVLink frame if it is a `TUNNEL` (id 385),
/// else `None`.
///
/// Reads the field straight off the wire bytes rather than going through the
/// typed parser, because the typed parser rejects a private (unregistered)
/// `payload_type` as an unknown enum value — exactly the values this pipe
/// carries. Handles a v2 frame; the standard dialect only emits TUNNEL as v2.
/// Returns `None` when the frame is not a v2 TUNNEL or is too short to hold the
/// `payload_type` field.
pub fn tunnel_payload_type(frame: &[u8]) -> Option<u16> {
    // v2 frame: STX 0xFD, then a 9-byte header (len, incompat, compat, seq,
    // sysid, compid, 3-byte msgid), so the payload begins at byte 10. The msgid
    // is the little-endian 24-bit field at bytes 7..10.
    if frame.first() != Some(&0xFD) || frame.len() < 10 {
        return None;
    }
    let mut id = [0u8; 4];
    id[..3].copy_from_slice(&frame[7..10]);
    if u32::from_le_bytes(id) != MSG_ID_TUNNEL {
        return None;
    }
    // payload_type is the first payload field (u16-LE) at bytes 10..12. A frame
    // truncated below that (an empty/short payload) is not a usable TUNNEL.
    if frame.len() < 12 {
        return None;
    }
    Some(u16::from_le_bytes([frame[10], frame[11]]))
}

/// The opaque application `payload` carried by a `TUNNEL` frame, reconstructed
/// to its declared length. The symmetric decode of [`build_tunnel_v2`]: it
/// reads the `payload_length` field and zero-pads the (MAVLink2
/// trailing-zero-truncated) wire bytes back to that length, so a caller gets
/// exactly the `payload` slice that was built, even when the tail of that
/// payload was zeros the transport truncated. Returns `None` when the frame is
/// not a well-formed v2 TUNNEL.
///
/// The TUNNEL payload begins after the v2 header (10 bytes) and the fixed
/// fields `payload_type`(u16) + `target_system`(u8) + `target_component`(u8) +
/// `payload_length`(u8), i.e. at body offset 5. The declared length is the
/// `payload_length` byte; the present wire bytes are however many survived
/// trailing-zero truncation, and the rest are zeros.
#[must_use]
pub fn tunnel_payload(frame: &[u8]) -> Option<Vec<u8>> {
    tunnel_payload_type(frame)?;
    // frame = 10-byte v2 header, then `body_len` body bytes, then a 2-byte CRC.
    let body_len = frame[1] as usize;
    let body_end = 10usize.checked_add(body_len)?;
    if frame.len() < body_end + 2 {
        return None;
    }
    let body = &frame[10..body_end];
    // body = payload_type(2) + target_system(1) + target_component(1) +
    // payload_length(1) + payload… — the payload starts at body offset 5. When
    // the payload (and its length byte) are all zeros, MAVLink2 trailing-zero
    // truncation drops them, so an absent length byte reads as a declared 0.
    let declared = body.get(4).copied().unwrap_or(0) as usize;
    let present = body.get(5..).unwrap_or(&[]);
    if present.len() > declared {
        // A payload longer on the wire than its own declared length is
        // malformed for this builder's frames.
        return None;
    }
    let mut payload = present.to_vec();
    payload.resize(declared, 0);
    Some(payload)
}
