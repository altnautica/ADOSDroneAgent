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
//! carries a normal UDP-sized datagram, so a request fits one packet for every
//! endpoint the relay serves, and a response fragments across as few frames as
//! the body needs. The plugin msgpack envelope is heavier than this lane
//! should pay per frame at the rates a small set of HTTP calls warrant.
//!
//! ## Wire layout
//!
//! Both frames travel as the aux payload (after the 6-byte aux header). All
//! multi-byte integers are big-endian, matching the aux header's length field
//! and the rest of the agent's framed IPC.
//!
//! ```text
//! RpcRequest
//!   byte 0              method (u8: 1=GET, 2=POST, 3=PUT, 4=DELETE)
//!   byte 1..5           id (u32 BE) — correlates the response
//!   byte 5              target_len (u8; 0 = broadcast, accepted by every drone)
//!   byte 6..6+T         target device id (UTF-8, <= MAX_DEVICE_ID)
//!   byte 6+T..8+T       path_len (u16 BE)
//!   byte 8+T..8+T+N     path bytes (UTF-8, not validated here)
//!   byte 8+T+N..10+T+N  body_len (u16 BE)
//!   byte 10+T+N..       body bytes
//!
//! RpcResponseFragment
//!   byte 0..4     id (u32 BE) — matches the request's
//!   byte 4..6     status (u16 BE) — HTTP status, repeated identically on every
//!                 fragment so a reassembler can seed from whichever arrives first
//!   byte 6..8     frag_index (u16 BE, 0-based)
//!   byte 8..10    frag_total (u16 BE, >= 1)
//!   byte 10..12   frag_len (u16 BE) — bytes in THIS fragment
//!   byte 12..     fragment bytes
//! ```
//!
//! ## Requests are single-frame; responses fragment
//!
//! Every agent write fits one frame: the largest is `PUT /api/config`, whose
//! `ConfigUpdate` body is `{"key":"...","value":"..."}` — tens of bytes against
//! a ~1130-byte budget. Request fragmentation would double the codec surface
//! for an endpoint that does not exist, so it is deliberately absent.
//!
//! Responses do not fit: measured against a live drone, `/api/services` is
//! 2 631 B, `/api/config` 5 101 B, and `/api/status/full` 29 339 B, all past the
//! 1 188-byte fragment budget. A response is therefore chunked by
//! [`split_response`] and reassembled on the ground.
//!
//! ## Why `AUX_VERSION` is not bumped for this layout change
//!
//! `aux_mux::AUX_VERSION` guards every aux channel. Bumping it would make a
//! mid-upgrade mixed pair drop MAVLink, Status, and Identity frames too. An
//! RPC-only layout mismatch during the upgrade window degrades to a 504 on
//! relay calls alone, and self-heals when the second rig restarts. That is
//! strictly the smaller blast radius, so the version byte stays put.
//!
//! ## Correlation
//!
//! The 32-bit request id is assigned by the ground side and travels both
//! directions. A ground station running one proxy caller can use a monotonic
//! counter; a 32-bit space at a few calls per second does not roll in any
//! realistic session. Every response fragment carries the same id back
//! unchanged, so a pending-request map on the ground can match a fragment to
//! its caller without sequencing across radio reordering.

use crate::aux_mux::AUX_MAX_PAYLOAD;
use crate::node_status::MAX_DEVICE_ID;

/// Fixed request overhead with an empty target: 1 byte method, 4 id,
/// 1 target_len, 2 path_len, 2 body_len.
///
/// The target, path, and body share what is left of [`AUX_MAX_PAYLOAD`] (the
/// cap `aux_mux::encode` enforces on every channel's payload);
/// [`encode_request`] rejects anything that does not fit one frame.
pub const RPC_REQUEST_OVERHEAD_BASE: usize = 10;

/// Largest body one response fragment can carry (12 bytes of fragment header).
pub const MAX_RESPONSE_FRAGMENT: usize = AUX_MAX_PAYLOAD - 12;

/// Most fragments one response may be split into.
///
/// 64 × [`MAX_RESPONSE_FRAGMENT`] = 76 032 B. The largest endpoint measured on
/// a live drone (`/api/status/full`, 29 339 B) needs 25, so this is real
/// headroom while still bounding a runaway reassembly buffer on the ground.
pub const MAX_RESPONSE_FRAGMENTS: usize = 64;

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
    /// The device id this request is addressed to. Empty means broadcast, and
    /// every drone on the pair answers it — the single-drone case and a bare
    /// diagnostic call both rely on that.
    pub target: &'a [u8],
    pub path: &'a [u8],
    pub body: &'a [u8],
}

/// A decoded response fragment. Borrows the original payload buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcResponse<'a> {
    pub id: u32,
    pub status: u16,
    /// 0-based position of this fragment within the response.
    pub index: u16,
    /// Total fragment count for this response; always >= 1.
    pub total: u16,
    /// This fragment's slice of the body, not the whole body.
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
/// `target` is the device id the request is addressed to; an empty slice is a
/// broadcast every linked drone answers.
///
/// Returns `None` when the target exceeds [`MAX_DEVICE_ID`] or the encoded
/// frame does not fit one aux datagram. Callers surface a 413 to their HTTP
/// client rather than truncating, because a truncated frame would fail
/// decoding on the far side and read as radio corruption rather than our bug.
pub fn encode_request(
    method: RpcMethod,
    id: u32,
    target: &[u8],
    path: &[u8],
    body: &[u8],
) -> Option<Vec<u8>> {
    if target.len() > MAX_DEVICE_ID {
        return None;
    }
    if path.len() > u16::MAX as usize {
        return None;
    }
    if body.len() > u16::MAX as usize {
        return None;
    }
    let total = RPC_REQUEST_OVERHEAD_BASE + target.len() + path.len() + body.len();
    if total > AUX_MAX_PAYLOAD {
        return None;
    }
    let mut out = Vec::with_capacity(total);
    out.push(method as u8);
    out.extend_from_slice(&id.to_be_bytes());
    out.push(target.len() as u8);
    out.extend_from_slice(target);
    out.extend_from_slice(&(path.len() as u16).to_be_bytes());
    out.extend_from_slice(path);
    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    out.extend_from_slice(body);
    Some(out)
}

/// Decode an aux payload as a request.
pub fn decode_request(payload: &[u8]) -> Result<RpcRequest<'_>, RpcCodecError> {
    if payload.len() < RPC_REQUEST_OVERHEAD_BASE {
        return Err(RpcCodecError::TooShort);
    }
    let method = RpcMethod::from_u8(payload[0]).ok_or(RpcCodecError::BadMethod(payload[0]))?;
    let id = u32::from_be_bytes([payload[1], payload[2], payload[3], payload[4]]);
    let target_len = payload[5] as usize;
    // 6 fixed bytes consumed; need 6 + target_len + 2 (path_len) <= payload.len()
    let path_len_offset = 6 + target_len;
    if path_len_offset + 2 > payload.len() {
        return Err(RpcCodecError::LengthMismatch {
            declared: path_len_offset + 2,
            actual: payload.len(),
        });
    }
    let target = &payload[6..path_len_offset];
    let path_len =
        u16::from_be_bytes([payload[path_len_offset], payload[path_len_offset + 1]]) as usize;
    let body_len_offset = path_len_offset + 2 + path_len;
    if body_len_offset + 2 > payload.len() {
        return Err(RpcCodecError::LengthMismatch {
            declared: body_len_offset + 2,
            actual: payload.len(),
        });
    }
    let path = &payload[path_len_offset + 2..body_len_offset];
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
        target,
        path,
        body,
    })
}

/// Split a response body into the fragments that will carry it.
///
/// An empty body yields exactly one empty chunk, so a 204 still travels as
/// `total = 1` and the ground side completes the call instead of waiting for a
/// fragment that never comes. Returns `None` when the body needs more than
/// [`MAX_RESPONSE_FRAGMENTS`] — the caller emits a 413 rather than a body the
/// ground cannot bound.
pub fn split_response(body: &[u8]) -> Option<Vec<&[u8]>> {
    if body.is_empty() {
        return Some(vec![&body[..0]]);
    }
    let count = body.len().div_ceil(MAX_RESPONSE_FRAGMENT);
    if count > MAX_RESPONSE_FRAGMENTS {
        return None;
    }
    Some(body.chunks(MAX_RESPONSE_FRAGMENT).collect())
}

/// Encode one response fragment as an aux payload.
///
/// Returns `None` when the fragment does not fit one aux datagram, when
/// `total` is zero, or when `index` is not inside `total`.
pub fn encode_response_fragment(
    id: u32,
    status: u16,
    index: u16,
    total: u16,
    chunk: &[u8],
) -> Option<Vec<u8>> {
    if total == 0 || index >= total {
        return None;
    }
    if chunk.len() > u16::MAX as usize {
        return None;
    }
    let encoded_len = 12 + chunk.len();
    if encoded_len > AUX_MAX_PAYLOAD {
        return None;
    }
    let mut out = Vec::with_capacity(encoded_len);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&status.to_be_bytes());
    out.extend_from_slice(&index.to_be_bytes());
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&(chunk.len() as u16).to_be_bytes());
    out.extend_from_slice(chunk);
    Some(out)
}

/// Decode an aux payload as a response fragment.
pub fn decode_response(payload: &[u8]) -> Result<RpcResponse<'_>, RpcCodecError> {
    if payload.len() < 12 {
        return Err(RpcCodecError::TooShort);
    }
    let id = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let status = u16::from_be_bytes([payload[4], payload[5]]);
    let index = u16::from_be_bytes([payload[6], payload[7]]);
    let total = u16::from_be_bytes([payload[8], payload[9]]);
    let frag_len = u16::from_be_bytes([payload[10], payload[11]]) as usize;
    if 12 + frag_len != payload.len() {
        return Err(RpcCodecError::LengthMismatch {
            declared: 12 + frag_len,
            actual: payload.len(),
        });
    }
    if total == 0 || index >= total {
        return Err(RpcCodecError::LengthMismatch {
            declared: total as usize,
            actual: index as usize,
        });
    }
    let body = &payload[12..12 + frag_len];
    Ok(RpcResponse {
        id,
        status,
        index,
        total,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_typical_get_request() {
        let path = b"/api/pairing/info";
        let enc = encode_request(RpcMethod::Get, 0xCAFEBABE, b"77735cd38937", path, &[]).unwrap();
        let dec = decode_request(&enc).unwrap();
        assert_eq!(dec.id, 0xCAFEBABE);
        assert_eq!(dec.method, RpcMethod::Get);
        assert_eq!(dec.target, b"77735cd38937");
        assert_eq!(dec.path, path);
        assert!(dec.body.is_empty());
    }

    #[test]
    fn round_trips_a_broadcast_request_with_an_empty_target() {
        let enc = encode_request(RpcMethod::Get, 5, &[], b"/api/status", &[]).unwrap();
        let dec = decode_request(&enc).unwrap();
        assert!(dec.target.is_empty(), "empty target means broadcast");
        assert_eq!(dec.path, b"/api/status");
    }

    #[test]
    fn rejects_a_target_longer_than_a_device_id() {
        let too_long = vec![b'a'; MAX_DEVICE_ID + 1];
        assert!(encode_request(RpcMethod::Get, 1, &too_long, b"/api/x", &[]).is_none());
        let fits = vec![b'a'; MAX_DEVICE_ID];
        let enc = encode_request(RpcMethod::Get, 1, &fits, b"/api/x", &[]).unwrap();
        assert_eq!(decode_request(&enc).unwrap().target, &fits[..]);
    }

    #[test]
    fn round_trips_a_post_with_a_body() {
        let path = b"/api/config";
        let body = br#"{"key":"agent.name","value":"skynodea7s"}"#;
        let enc = encode_request(RpcMethod::Post, 1, b"77735cd38937", path, body).unwrap();
        let dec = decode_request(&enc).unwrap();
        assert_eq!(dec.method, RpcMethod::Post);
        assert_eq!(dec.path, path);
        assert_eq!(dec.body, body);
    }

    #[test]
    fn round_trips_a_single_fragment_response() {
        let body = br#"{"device_id":"abc","name":"Skynode A7S"}"#;
        let chunks = split_response(body).unwrap();
        assert_eq!(chunks.len(), 1);
        let enc = encode_response_fragment(42, 200, 0, 1, chunks[0]).unwrap();
        let dec = decode_response(&enc).unwrap();
        assert_eq!(dec.id, 42);
        assert_eq!(dec.status, 200);
        assert_eq!(dec.index, 0);
        assert_eq!(dec.total, 1);
        assert_eq!(dec.body, body);
    }

    #[test]
    fn round_trips_an_empty_path_empty_body_request() {
        let enc = encode_request(RpcMethod::Get, 0, &[], &[], &[]).unwrap();
        let dec = decode_request(&enc).unwrap();
        assert_eq!(dec.id, 0);
        assert_eq!(dec.method, RpcMethod::Get);
        assert!(dec.path.is_empty());
        assert!(dec.body.is_empty());
    }

    #[test]
    fn an_empty_body_still_produces_one_fragment() {
        // A 204 must complete the caller, not leave it waiting for a fragment
        // that never comes.
        let chunks = split_response(&[]).unwrap();
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].is_empty());
        let enc = encode_response_fragment(7, 204, 0, 1, chunks[0]).unwrap();
        let dec = decode_response(&enc).unwrap();
        assert_eq!(dec.status, 204);
        assert_eq!(dec.total, 1);
        assert!(dec.body.is_empty());
    }

    #[test]
    fn a_three_kilobyte_body_splits_into_three_fragments_that_rejoin() {
        let body: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        let chunks = split_response(&body).unwrap();
        assert_eq!(chunks.len(), 3);
        let total = chunks.len() as u16;
        let mut rejoined = Vec::new();
        for (i, chunk) in chunks.iter().enumerate() {
            let enc = encode_response_fragment(1, 200, i as u16, total, chunk).unwrap();
            let dec = decode_response(&enc).unwrap();
            assert_eq!(dec.total, total);
            assert_eq!(dec.index, i as u16);
            assert_eq!(dec.status, 200);
            rejoined.extend_from_slice(dec.body);
        }
        assert_eq!(rejoined, body);
    }

    #[test]
    fn a_body_past_the_reassembly_ceiling_does_not_split() {
        let body = vec![0u8; 77_000];
        assert!(split_response(&body).is_none());
        // The largest body that still fits fragments exactly to the ceiling.
        let at_ceiling = vec![0u8; MAX_RESPONSE_FRAGMENT * MAX_RESPONSE_FRAGMENTS];
        assert_eq!(
            split_response(&at_ceiling).unwrap().len(),
            MAX_RESPONSE_FRAGMENTS
        );
    }

    #[test]
    fn rejects_a_request_larger_than_one_aux_frame() {
        let budget = AUX_MAX_PAYLOAD - RPC_REQUEST_OVERHEAD_BASE;
        let big_path = vec![b'A'; budget + 1];
        assert!(encode_request(RpcMethod::Get, 1, &[], &big_path, &[]).is_none());
        // The boundary case fits and decodes
        let fits = vec![b'A'; budget];
        let enc = encode_request(RpcMethod::Get, 1, &[], &fits, &[]).unwrap();
        let dec = decode_request(&enc).unwrap();
        assert_eq!(dec.path.len(), budget);
    }

    #[test]
    fn rejects_a_fragment_larger_than_one_aux_frame() {
        let big = vec![0u8; MAX_RESPONSE_FRAGMENT + 1];
        assert!(encode_response_fragment(1, 200, 0, 1, &big).is_none());
        let fits = vec![0u8; MAX_RESPONSE_FRAGMENT];
        let enc = encode_response_fragment(1, 200, 0, 1, &fits).unwrap();
        assert_eq!(
            decode_response(&enc).unwrap().body.len(),
            MAX_RESPONSE_FRAGMENT
        );
    }

    #[test]
    fn rejects_a_fragment_whose_index_is_outside_its_total() {
        assert!(encode_response_fragment(1, 200, 3, 3, b"x").is_none());
        assert!(encode_response_fragment(1, 200, 0, 0, b"x").is_none());
        // Hand-built frame claiming index 4 of 2 must not decode.
        let mut bad = Vec::new();
        bad.extend_from_slice(&1u32.to_be_bytes());
        bad.extend_from_slice(&200u16.to_be_bytes());
        bad.extend_from_slice(&4u16.to_be_bytes());
        bad.extend_from_slice(&2u16.to_be_bytes());
        bad.extend_from_slice(&1u16.to_be_bytes());
        bad.push(b'x');
        assert!(decode_response(&bad).is_err());
    }

    #[test]
    fn rejects_a_truncated_request() {
        let full = encode_request(RpcMethod::Post, 9, b"abc", b"/api/x", b"payload").unwrap();
        let mut truncated = full.clone();
        truncated.truncate(5);
        assert_eq!(
            decode_request(&truncated).unwrap_err(),
            RpcCodecError::TooShort
        );
    }

    #[test]
    fn rejects_a_truncated_response() {
        let full = encode_response_fragment(9, 200, 0, 1, b"body").unwrap();
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
        // Payload is 10 bytes (method + id + target_len + path_len + body_len),
        // which passes the TooShort gate, then the body_len_offset check
        // catches the lie.
        let mut bad = vec![RpcMethod::Get as u8];
        bad.extend_from_slice(&1u32.to_be_bytes());
        bad.push(0); // target_len
        bad.extend_from_slice(&100u16.to_be_bytes()); // path_len claims 100
        bad.extend_from_slice(&0u16.to_be_bytes()); // body_len claims 0
        assert_eq!(
            decode_request(&bad).unwrap_err(),
            RpcCodecError::LengthMismatch {
                declared: 110,
                actual: 10
            }
        );
    }

    #[test]
    fn catches_a_length_mismatch_in_a_response() {
        // Claimed frag_len = 50 but the payload ends after the header.
        let mut bad = Vec::new();
        bad.extend_from_slice(&1u32.to_be_bytes());
        bad.extend_from_slice(&200u16.to_be_bytes());
        bad.extend_from_slice(&0u16.to_be_bytes());
        bad.extend_from_slice(&1u16.to_be_bytes());
        bad.extend_from_slice(&50u16.to_be_bytes());
        assert_eq!(
            decode_response(&bad).unwrap_err(),
            RpcCodecError::LengthMismatch {
                declared: 62,
                actual: 12
            }
        );
    }

    #[test]
    fn rejects_an_unknown_method_byte() {
        let mut bad = vec![0xFFu8]; // unknown method
        bad.extend_from_slice(&1u32.to_be_bytes());
        bad.push(0); // target_len
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
    fn frame_budgets_account_for_fixed_overhead() {
        // Request overhead with an empty target is 10 bytes (1 method + 4 id +
        // 1 target_len + 2 path_len + 2 body_len); a response fragment's is 12
        // (4 id + 2 status + 2 index + 2 total + 2 frag_len).
        assert_eq!(RPC_REQUEST_OVERHEAD_BASE, 10);
        assert_eq!(MAX_RESPONSE_FRAGMENT, 1200 - 12);
        // The ceiling must cover the largest endpoint measured on a live drone
        // (/api/status/full at 29 339 B) with headroom.
        const { assert!(MAX_RESPONSE_FRAGMENT * MAX_RESPONSE_FRAGMENTS > 29_339) };
    }
}
