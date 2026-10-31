//! COMMAND_LONG frame building.

use super::*;

/// MAVLink message id of `COMMAND_LONG` (`0x4C`).
pub const MSG_ID_COMMAND_LONG: u32 = 76;

/// The `COMMAND_LONG` CRC_EXTRA, the message-definition seed the X.25 checksum
/// folds in last. This is the value the canonical dialect carries for the message
/// (the same value the generated codec and the reference Python encoder use);
/// it is NOT the message id.
pub const COMMAND_LONG_CRC_EXTRA: u8 = 152;

/// Build a complete MAVLink v2 `COMMAND_LONG` frame for an ARBITRARY `command`
/// id, returning the raw frame bytes ready to write to the MAVLink socket.
///
/// The generated dialect enum only carries named command ids, so a `COMMAND_LONG`
/// for a command the enum does not name cannot be built through the typed
/// `MavMessage::COMMAND_LONG` path. This serializes the wire frame directly: the
/// `COMMAND_LONG` payload (the seven `param`s as little-endian f32, then the
/// `command` u16, then `target_system` / `target_component` / `confirmation` as
/// u8, in wire order), MAVLink2 trailing-zero truncation, the v2 header (both the
/// incompat and compat flags are 0), and the X.25 checksum folded with
/// [`COMMAND_LONG_CRC_EXTRA`]. Identical on the wire to a `COMMAND_LONG` for a
/// named id; the only difference is this accepts an id the enum has no variant
/// for.
///
/// `confirmation` is fixed at 0 (the command surface is fire-and-forget). The
/// frame is unsigned (no MAVLink2 signature; incompat flags = 0).
#[allow(clippy::too_many_arguments)]
pub fn build_command_long_v2(
    header: MavHeader,
    command: u16,
    target_system: u8,
    target_component: u8,
    params: [f32; 7],
) -> Vec<u8> {
    // The COMMAND_LONG payload in wire (field-id) order: the seven f32 params
    // first (28 bytes), then the u16 command (2), then target_system,
    // target_component, confirmation (1 each) → 33 bytes max.
    let mut payload = Vec::with_capacity(33);
    for p in params {
        payload.extend_from_slice(&p.to_le_bytes());
    }
    payload.extend_from_slice(&command.to_le_bytes());
    payload.push(target_system);
    payload.push(target_component);
    payload.push(0u8); // confirmation

    // MAVLink2 truncates trailing zero bytes off the payload, keeping at least
    // one byte. The CRC is computed over the truncated payload.
    truncate_trailing_zeros(&mut payload);

    let mut frame = Vec::with_capacity(10 + payload.len() + 2);
    frame.push(0xFD); // v2 start-of-frame
    frame.push(payload.len() as u8); // payload length
    frame.push(0x00); // incompat flags (unsigned)
    frame.push(0x00); // compat flags
    frame.push(header.sequence);
    frame.push(header.system_id);
    frame.push(header.component_id);
    // 3-byte little-endian message id.
    frame.push((MSG_ID_COMMAND_LONG & 0xFF) as u8);
    frame.push(((MSG_ID_COMMAND_LONG >> 8) & 0xFF) as u8);
    frame.push(((MSG_ID_COMMAND_LONG >> 16) & 0xFF) as u8);
    frame.extend_from_slice(&payload);

    // X.25 checksum over every byte after the start-of-frame, then the CRC_EXTRA.
    let mut crc = X25_INIT;
    for &b in &frame[1..] {
        crc = x25_accumulate(b, crc);
    }
    crc = x25_accumulate(COMMAND_LONG_CRC_EXTRA, crc);
    frame.push((crc & 0xFF) as u8);
    frame.push((crc >> 8) as u8);

    frame
}
