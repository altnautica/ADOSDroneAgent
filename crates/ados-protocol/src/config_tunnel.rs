//! Identifiers of the config-over-radio channel carried in MAVLink TUNNEL
//! frames. The chunking itself is the `ados-mavlink-tunnel` crate; these are
//! the values that make a TUNNEL frame belong to this channel.

use crate::mavlink::TUNNEL_RESERVED_PAYLOAD_TYPE_MAX;

/// Private `payload_type` tagging every TUNNEL frame of the config-over-radio
/// channel. A single type carries both directions; the request/response
/// distinction is the `is_response` flag in the `ados-mavlink-tunnel` chunk
/// header, so a demux needs one `tunnel_payload_type` compare.
pub const CONFIG_TUNNEL_PAYLOAD_TYPE: u16 = 0xADC0;

/// Chunk-header magic of the config-over-radio channel (the
/// `ados_mavlink_tunnel::Framing` magic). A plugin holding the
/// `mavlink.tunnel` capability can mint a TUNNEL with any private
/// payload_type, so the reassembler checks this magic and ignores unrelated
/// traffic on the same type.
pub const CONFIG_TUNNEL_MAGIC: [u8; 2] = [0xAD, 0x03];

const _: () = assert!(CONFIG_TUNNEL_PAYLOAD_TYPE > TUNNEL_RESERVED_PAYLOAD_TYPE_MAX);
