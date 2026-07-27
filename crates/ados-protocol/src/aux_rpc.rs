//! Bidirectional RPC frames over the auxiliary lane.
//!
//! [`aux_mux`](crate::aux_mux) says what an aux datagram means; this module is
//! what a relay-proxy request and response look like on the application lane.
//! The ground station, paired to a drone only through WFB, has no IP reach to
//! the linked drone; `AuxEgress` carries low-rate framing onto the radio
//! uplink, and the drone's `aux_uplink_consumer` reads it. Here we define two
//! payload shapes that ride that pair, so a ground station's `relay-proxy`
//! HTTP route can forward `GET /api/pairing/info` (and the rest of the
//! agent's own HTTP surface) to a drone it cannot address directly.
//!
//! ## Why not reuse an existing envelope
//!
//! The MAVLink TUNNEL extension (`crate::tunnel_config`) chunks a single body
//! across 128-byte frames, sized for the radio's control plane. The aux lane
//! carries a normal UDP-sized datagram, so a request/response pair fits one
//! packet for the payloads that matter here (the 19-field `/api/pairing/info`,
//! `/api/status`, `/api/services`, `/api/system`) and beats chunked transport
//! on round-trip latency. The plugin msgpack envelope is heavier than this
//! lane should pay per frame at the rates a small set of HTTP calls warrant.
//!
//! ## Wire layout
//!
//! Both frames travel as the aux payload (after the 6-byte aux header). All
//! multi-byte integers are big-endian, matching the aux header's length field
//! and the rest of the agent's framed IPC.
//!
//! ```text
//! RpcRequest
//!   byte 0          method (u8: 1=GET, 2=POST, 3=PUT, 4=DELETE)
//!   byte 1..5       id (u32 BE) — correlates the response
//!   byte 5..7       path_len (u16 BE)
//!   byte 7..7+N     path bytes (UTF-8, not validated here)
//!   byte 7+N..9+N   body_len (u16 BE)
//!   byte 9+N..9+N+M body bytes
//!
//! RpcResponse
//!   byte 0..4       id (u32 BE) — matches the request's
//!   byte 4..6       status (u16 BE) — HTTP status code
//!   byte 6..8       body_len (u16 BE)
//!   byte 8..8+M     body bytes
//! ```
//!
//! Request overhead is 9 bytes; response overhead is 8. Both bounded well
//! under [`AUX_MAX_PAYLOAD`] = 1200, so a typical `pairing/info` (~700 bytes)
//! fits one frame. A payload larger than the frame budget is rejected by the
//! encoder — the caller surfaces a 413 to its HTTP client, never a truncated
//! frame that decodes as corruption on the far side.
//!
//! ## Correlation
//!
//! The 32-bit request id is assigned by the ground side and travels both
//! directions. A ground station running one proxy caller can use a monotonic
//! counter; a 32-bit space at a few calls per second does not roll in any
//! realistic session. The drone's response carries the same id back unchanged,
//! so a pending-request map on the ground can match a response to its caller
//! without sequencing across radio reordering.

use crate::aux_mux::AUX_MAX_PAYLOAD;

/// Largest payload a request frame can carry.
///
/// Request fixed overhead is 9 bytes (1 method + 4 id + 2 path_len + 2
/// body_len); the path and body share what is left of [`AUX_MAX_PAYLOAD`]
/// (the cap `aux_mux::encode` enforces on every channel's payload). This is a
/// guidance cap for callers choosing what to forward; the encoder rejects
/// anything that does not fit as a single frame.
pub const MAX_REQUEST_PAYLOAD: usize = AUX_MAX_PAYLOAD - 9;

/// Largest payload a response frame can carry.
pub const MAX_RESPONSE_PAYLOAD: usize = AUX_MAX_PAYLOAD - 8;

/// HTTP method tag, encoded as a single byte on the wire.
///
/// Values are explicit and MUST NOT be renumbered: a ground station may run a
/// different agent build than its linked drone across an upgrade, and a
/// renumber would silently reroute a POST into a GET handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RpcMethod {
    Get = 1,
    Post = 2,
    Put = 3,
    Delete = 4,
}

impl RpcMethod {
    /// Parse a method byte. Unknown values return `None` so a reader drops
    /// the frame instead of guessing.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Get),
            2 => Some(Self::Post),
            3 => Some(Self::Put),
            4 => Some(Self::Delete),
            _ => None,
        }
    }

    /// The HTTP method string, for a proxy that re-emits the request.
    pub fn as_http_method(&self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
        }
    }
}

/// A decoded request frame. Borrows the original payload buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcRequest<'a> {
    pub id: u32,
    pub method: RpcMethod,
    pub path: &'a [u8],
    pub body: &'a [u8],
}

/// A decoded response frame. Borrows the original payload buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcResponse<'a> {
    pub id: u32,
    pub status: u16,
    pub body: &'a [u8],
}

/// Why a frame could not be decoded. Distinct variants so a caller can count
/// transport damage separately from foreign traffic on a shared lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcCodecError {
    /// Shorter than the fixed header.
    TooShort,
    /// A method byte this build does not know.
    BadMethod(u8),
    /// The declared lengths do not match the bytes actually present.
    LengthMismatch { declared: usize, actual: usize },
}

/// Encode a request as an aux payload (the bytes that go inside the aux
/// frame AFTER the 6-byte aux header).
///
/// Returns `None` when the encoded frame does not fit one aux datagram.
/// Callers surface a 413 to their HTTP client rather than truncating,
/// because a truncated frame would fail decoding on the far side and read as
/// radio corruption rather than our bug.
pub fn encode_request(method: RpcMethod, id: u32, path: &[u8], body: &[u8]) -> Option<Vec<u8>> {
    if path.len() > u16::MAX as usize {
        return None;
    }
    if body.len() > u16::MAX as usize {
        return None;
    }
    let total = 9 + path.len() + body.len();
    if total > AUX_MAX_PAYLOAD {
        return None;
    }
    let mut out = Vec::with_capacity(total);
    out.push(method as u8);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&(path.len() as u16).to_be_bytes());
    out.extend_from_slice(path);
    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    out.extend_from_slice(body);
    Some(out)
}

/// Decode an aux payload as a request.
pub fn decode_request(payload: &[u8]) -> Result<RpcRequest<'_>, RpcCodecError> {
    if payload.len() < 9 {
        return Err(RpcCodecError::TooShort);
    }
    let method = RpcMethod::from_u8(payload[0]).ok_or(RpcCodecError::BadMethod(payload[0]))?;
    let id = u32::from_be_bytes([payload[1], payload[2], payload[3], payload[4]]);
    let path_len = u16::from_be_bytes([payload[5], payload[6]]) as usize;
    // 7 bytes for fixed fields already consumed; need 7 + path_len + 2 (body_len) <= payload.len()
    let body_len_offset = 7 + path_len;
    if body_len_offset + 2 > payload.len() {
        return Err(RpcCodecError::LengthMismatch {
            declared: body_len_offset + 2,
            actual: payload.len(),
        });
    }
    let path = &payload[7..body_len_offset];
    let body_len =
        u16::from_be_bytes([payload[body_len_offset], payload[body_len_offset + 1]]) as usize;
    let body_start = body_len_offset + 2;
    if body_start + body_len != payload.len() {
        return Err(RpcCodecError::LengthMismatch {
            declared: body_start + body_len,
            actual: payload.len(),
        });
    }
    let body = &payload[body_start..body_start + body_len];
    Ok(RpcRequest {
        id,
        method,
        path,
        body,
    })
}

/// Encode a response as an aux payload.
///
/// Returns `None` when the encoded frame does not fit one aux datagram.
pub fn encode_response(id: u32, status: u16, body: &[u8]) -> Option<Vec<u8>> {
    if body.len() > u16::MAX as usize {
        return None;
    }
    let total = 8 + body.len();
    if total > AUX_MAX_PAYLOAD {
        return None;
    }
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&status.to_be_bytes());
    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    out.extend_from_slice(body);
    Some(out)
}

/// Decode an aux payload as a response.
pub fn decode_response(payload: &[u8]) -> Result<RpcResponse<'_>, RpcCodecError> {
    if payload.len() < 8 {
        return Err(RpcCodecError::TooShort);
    }
    let id = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let status = u16::from_be_bytes([payload[4], payload[5]]);
    let body_len = u16::from_be_bytes([payload[6], payload[7]]) as usize;
    if 8 + body_len != payload.len() {
        return Err(RpcCodecError::LengthMismatch {
            declared: 8 + body_len,
            actual: payload.len(),
        });
    }
    let body = &payload[8..8 + body_len];
    Ok(RpcResponse { id, status, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_typical_get_request() {
        let path = b"/api/pairing/info";
        let enc = encode_request(RpcMethod::Get, 0xCAFEBABE, path, &[]).unwrap();
        let dec = decode_request(&enc).unwrap();
        assert_eq!(dec.id, 0xCAFEBABE);
        assert_eq!(dec.method, RpcMethod::Get);
        assert_eq!(dec.path, path);
        assert!(dec.body.is_empty());
    }

    #[test]
    fn round_trips_a_post_with_a_body() {
        let path = b"/api/config";
        let body = br#"{"wifi_client":{"ssid":"home","psk":"secret"}}"#;
        let enc = encode_request(RpcMethod::Post, 1, path, body).unwrap();
        let dec = decode_request(&enc).unwrap();
        assert_eq!(dec.method, RpcMethod::Post);
        assert_eq!(dec.path, path);
        assert_eq!(dec.body, body);
    }

    #[test]
    fn round_trips_a_response_with_a_body() {
        let body = br#"{"device_id":"abc","name":"Skynode A7S"}"#;
        let enc = encode_response(42, 200, body).unwrap();
        let dec = decode_response(&enc).unwrap();
        assert_eq!(dec.id, 42);
        assert_eq!(dec.status, 200);
        assert_eq!(dec.body, body);
    }

    #[test]
    fn round_trips_an_empty_path_empty_body_request() {
        let enc = encode_request(RpcMethod::Get, 0, &[], &[]).unwrap();
        let dec = decode_request(&enc).unwrap();
        assert_eq!(dec.id, 0);
        assert_eq!(dec.method, RpcMethod::Get);
        assert!(dec.path.is_empty());
        assert!(dec.body.is_empty());
    }

    #[test]
    fn round_trips_an_empty_body_response() {
        let enc = encode_response(7, 204, &[]).unwrap();
        let dec = decode_response(&enc).unwrap();
        assert_eq!(dec.id, 7);
        assert_eq!(dec.status, 204);
        assert!(dec.body.is_empty());
    }

    #[test]
    fn rejects_a_request_larger_than_one_aux_frame() {
        let big_path = vec![b'A'; MAX_REQUEST_PAYLOAD + 1];
        assert!(encode_request(RpcMethod::Get, 1, &big_path, &[]).is_none());
        // The boundary case fits and decodes
        let fits = vec![b'A'; MAX_REQUEST_PAYLOAD];
        let enc = encode_request(RpcMethod::Get, 1, &fits, &[]).unwrap();
        let dec = decode_request(&enc).unwrap();
        assert_eq!(dec.path.len(), MAX_REQUEST_PAYLOAD);
    }

    #[test]
    fn rejects_a_response_larger_than_one_aux_frame() {
        let big_body = vec![0u8; MAX_RESPONSE_PAYLOAD + 1];
        assert!(encode_response(1, 200, &big_body).is_none());
        // The boundary case fits and decodes
        let fits = vec![0u8; MAX_RESPONSE_PAYLOAD];
        let enc = encode_response(1, 200, &fits).unwrap();
        let dec = decode_response(&enc).unwrap();
        assert_eq!(dec.body.len(), MAX_RESPONSE_PAYLOAD);
    }

    #[test]
    fn rejects_a_truncated_request() {
        let full = encode_request(RpcMethod::Post, 9, b"/api/x", b"payload").unwrap();
        let mut truncated = full.clone();
        truncated.truncate(5);
        assert_eq!(
            decode_request(&truncated).unwrap_err(),
            RpcCodecError::TooShort
        );
    }

    #[test]
    fn rejects_a_truncated_response() {
        let full = encode_response(9, 200, b"body").unwrap();
        let mut truncated = full.clone();
        truncated.truncate(4);
        assert_eq!(
            decode_response(&truncated).unwrap_err(),
            RpcCodecError::TooShort
        );
    }

    #[test]
    fn catches_a_length_mismatch_in_a_request() {
        // Path length declared as 100 but no path bytes are actually present.
        // Payload is 9 bytes (method + id + path_len + body_len), which passes
        // the TooShort gate, then the body_len_offset check catches the lie.
        let mut bad = vec![RpcMethod::Get as u8];
        bad.extend_from_slice(&1u32.to_be_bytes());
        bad.extend_from_slice(&100u16.to_be_bytes()); // path_len claims 100
        bad.extend_from_slice(&0u16.to_be_bytes()); // body_len claims 0
        assert_eq!(
            decode_request(&bad).unwrap_err(),
            RpcCodecError::LengthMismatch {
                declared: 109,
                actual: 9
            }
        );
    }

    #[test]
    fn catches_a_length_mismatch_in_a_response() {
        // Claimed body_len = 50 but the payload ends after the header.
        let mut bad = Vec::new();
        bad.extend_from_slice(&1u32.to_be_bytes());
        bad.extend_from_slice(&200u16.to_be_bytes());
        bad.extend_from_slice(&50u16.to_be_bytes());
        assert_eq!(
            decode_response(&bad).unwrap_err(),
            RpcCodecError::LengthMismatch {
                declared: 58,
                actual: 8
            }
        );
    }

    #[test]
    fn rejects_an_unknown_method_byte() {
        let mut bad = vec![0xFFu8]; // unknown method
        bad.extend_from_slice(&1u32.to_be_bytes());
        bad.extend_from_slice(&0u16.to_be_bytes()); // path_len
        bad.extend_from_slice(&0u16.to_be_bytes()); // body_len
        assert_eq!(
            decode_request(&bad).unwrap_err(),
            RpcCodecError::BadMethod(0xFF)
        );
    }

    #[test]
    fn method_numbers_are_pinned() {
        // Both rigs may run different builds across an upgrade. Renumbering
        // would silently reroute a POST into a GET handler on the far side.
        assert_eq!(RpcMethod::Get as u8, 1);
        assert_eq!(RpcMethod::Post as u8, 2);
        assert_eq!(RpcMethod::Put as u8, 3);
        assert_eq!(RpcMethod::Delete as u8, 4);
        assert_eq!(RpcMethod::from_u8(1), Some(RpcMethod::Get));
        assert_eq!(RpcMethod::from_u8(0), None);
    }

    #[test]
    fn method_maps_to_an_http_verb_string() {
        assert_eq!(RpcMethod::Get.as_http_method(), "GET");
        assert_eq!(RpcMethod::Post.as_http_method(), "POST");
        assert_eq!(RpcMethod::Put.as_http_method(), "PUT");
        assert_eq!(RpcMethod::Delete.as_http_method(), "DELETE");
    }

    #[test]
    fn max_payloads_account_for_fixed_overhead() {
        // Request payload overhead is 9 bytes (1 method + 4 id + 2 path_len + 2 body_len);
        // response overhead is 8 bytes (4 id + 2 status + 2 body_len).
        // The encoder rejects anything whose total RPC payload exceeds AUX_MAX_PAYLOAD,
        // which is the cap aux_mux::encode enforces on every channel's payload.
        assert_eq!(MAX_REQUEST_PAYLOAD, 1200 - 9);
        assert_eq!(MAX_RESPONSE_PAYLOAD, 1200 - 8);
    }
}
