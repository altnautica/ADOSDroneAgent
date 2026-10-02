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
//! carries a normal UDP-sized datagram, so a request and a response each span
//! as few frames as their bytes need. The plugin msgpack envelope is heavier
//! than this lane should pay per frame at the rates a small set of HTTP calls
//! warrant.
//!
//! ## Wire layout
//!
//! Both frames travel as the aux payload (after the 6-byte aux header). All
//! multi-byte integers are big-endian, matching the aux header's length field
//! and the rest of the agent's framed IPC.
//!
//! ```text
//! RpcRequestFragment
//!   byte 0              method (u8: 1=GET, 2=POST, 3=PUT, 4=DELETE, 5=PATCH)
//!   byte 1..5           id (u32 BE) — correlates the response
//!   byte 5              target_len (u8; 0 = broadcast, accepted by every drone)
//!   byte 6..6+T         target device id (UTF-8, <= MAX_DEVICE_ID)
//!   byte 6+T..8+T       seq (u16 BE, 0-based)
//!   byte 8+T..10+T      total (u16 BE, >= 1)
//!   byte 10+T..         chunk bytes (the rest of the payload)
//!
//! The chunks, concatenated in `seq` order, are the REQUEST OBJECT:
//!   path_len (u16 BE) | path | content_type_len (u8) | content_type |
//!   ticket_len (u16 BE) | ticket | body (every remaining byte)
//!
//! RpcResponseFragment
//!   byte 0              sender_len (u8, <= MAX_DEVICE_ID)
//!   byte 1..1+S         sender device id (UTF-8) — which drone answered
//!   byte 1+S..5+S       id (u32 BE) — matches the request's
//!   byte 5+S..7+S       status (u16 BE) — HTTP status, repeated identically on
//!                       every fragment so a reassembler can seed from
//!                       whichever arrives first. The top bit
//!                       ([`response::RESPONSE_HEADERS_FLAG`]) is not part of
//!                       the status: it marks that the ENCODED OBJECT (not the
//!                       fragment) carries a trailing header block.
//!   byte 7+S..9+S       frag_index (u16 BE, 0-based; also the RaptorQ
//!                       encoding-symbol id)
//!   byte 9+S..11+S      frag_total (u16 BE, >= 1)
//!   byte 11+S..15+S     oti (u32 BE) — the RaptorQ transfer length, identical
//!                       on every fragment of one response
//!   byte 15+S..17+S     frag_len (u16 BE) — symbol bytes in THIS fragment
//!   byte 17+S..         symbol bytes
//! ```
//!
//! The bytes the fragments carry are not the body directly, they are an
//! **encoded object**. With no headers the object IS the body. With headers it
//! is `body | header entries | block_len (u32 BE)`, and the status's top bit
//! says so — see [`response::pack_response`]. Headers therefore cost nothing
//! per fragment and do not move [`response::MAX_RESPONSE_FRAGMENT`], which the
//! whole symbol geometry is derived from.
//!
//! ## Both directions fragment
//!
//! A request is split across as many frames as its object needs, up to
//! [`MAX_REQUEST_BODY`] of body. Plugin routes and larger writes (a mission, a
//! uploaded file, a non-JSON plugin payload) do not fit one 1.2 KB frame, and
//! a relay that refused them with a 413 left a radio-only drone without those
//! surfaces at all. Request fragments are plain chunks rather than RaptorQ
//! symbols: the ground retransmits a whole request it has no answer for, and
//! the drone's [`RequestReassembler`] keeps what already arrived.
//!
//! Responses fragment too: measured against a live drone, `/api/services` is
//! 2 631 B, `/api/config` 5 101 B, and `/api/status/full` 29 339 B, all past the
//! [`response::MAX_RESPONSE_FRAGMENT`] budget. A response is therefore
//! forward-error-corrected and chunked by [`split_response`]; see
//! [`response`] for why a fragment is a RaptorQ symbol and why it names its
//! sender.
//!
//! ## Version
//!
//! `aux_mux::AUX_VERSION` guards every aux channel. Bumping it for an RPC-only
//! layout would make a mid-upgrade mixed pair drop MAVLink, Status, and
//! Identity frames too, so the RPC layout changes without it: a mismatched
//! pair degrades to a 504 on relay calls alone until both ends run the same
//! build.
//!
//! ## Correlation
//!
//! The 32-bit request id is assigned by the ground side and travels both
//! directions. A ground station running one proxy caller can use a monotonic
//! counter; a 32-bit space at a few calls per second does not roll in any
//! realistic session. Every response fragment carries the same id back
//! unchanged, so a pending-request map on the ground can match a fragment to
//! its caller without sequencing across radio reordering. The id alone is not
//! enough to identify the answering drone once a fleet shares one key, which
//! is what the fragment's `sender` field is for.

pub mod response;

pub use response::{
    decode_response, encode_response_fragment, pack_response, split_response, unpack_response,
    FragmentOutcome, ResponseDecoder, ResponseHeader, ResponseSymbols, RpcResponse,
    MAX_RESPONSE_BODY, MAX_RESPONSE_FRAGMENT, MAX_RESPONSE_FRAGMENTS, MAX_RESPONSE_HEADER_BLOCK,
    RESPONSE_HEADERS_FLAG, RPC_REPAIR_SYMBOLS, RPC_RESPONSE_OVERHEAD_BASE,
};

use crate::aux_mux::AUX_MAX_PAYLOAD;
use crate::node_status::MAX_DEVICE_ID;

/// Fixed request-fragment overhead with an empty target: 1 byte method, 4 id,
/// 1 target_len, 2 seq, 2 total.
pub const RPC_REQUEST_FRAGMENT_OVERHEAD: usize = 10;

/// Chunk bytes one request fragment carries, sized so a worst-case device id
/// still fits [`AUX_MAX_PAYLOAD`].
pub const MAX_REQUEST_CHUNK: usize =
    AUX_MAX_PAYLOAD - RPC_REQUEST_FRAGMENT_OVERHEAD - MAX_DEVICE_ID;

/// Largest request body the relay carries. A larger one is refused with 413 on
/// the ground before anything is sent.
pub const MAX_REQUEST_BODY: usize = 1024 * 1024;

/// Largest relay ticket a request object can carry.
pub const MAX_REQUEST_TICKET: usize = 1024;

/// Largest request object: the body plus its path, content type and ticket.
pub const MAX_REQUEST_OBJECT: usize =
    MAX_REQUEST_BODY + 2 + u16::MAX as usize + 1 + u8::MAX as usize + 2 + MAX_REQUEST_TICKET;

/// Most fragments one request may span.
pub const MAX_REQUEST_FRAGMENTS: usize = MAX_REQUEST_OBJECT.div_ceil(MAX_REQUEST_CHUNK);

/// How long the drone keeps a partially received request before dropping it.
/// Longer than the ground's call bound, so a retransmit can still fill the
/// gaps of the attempt before it.
pub const REQUEST_REASSEMBLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Most requests the drone reassembles at once.
pub const MAX_PARTIAL_REQUESTS: usize = 8;

/// Most bytes the drone buffers across every partial request at once.
pub const MAX_PARTIAL_REQUEST_BYTES: usize = 4 * MAX_REQUEST_OBJECT;

/// HTTP method tag, encoded as a single byte on the wire.
///
/// Values are explicit and MUST NOT be renumbered: a renumber would silently
/// reroute a POST into a GET handler on the far side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RpcMethod {
    Get = 1,
    Post = 2,
    Put = 3,
    Delete = 4,
    Patch = 5,
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
            5 => Some(Self::Patch),
            _ => None,
        }
    }

    /// Parse an HTTP method name. `None` for a method the relay does not carry.
    pub fn from_http_method(method: &str) -> Option<Self> {
        match method {
            "GET" => Some(Self::Get),
            "POST" => Some(Self::Post),
            "PUT" => Some(Self::Put),
            "DELETE" => Some(Self::Delete),
            "PATCH" => Some(Self::Patch),
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
            Self::Patch => "PATCH",
        }
    }
}

/// A whole request, reassembled from its fragments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcRequest {
    pub id: u32,
    pub method: RpcMethod,
    /// The device id this request is addressed to. Empty means broadcast, and
    /// every drone on the pair answers it.
    pub target: Vec<u8>,
    /// Absolute path on the drone's API, still percent-encoded, with any query.
    pub path: Vec<u8>,
    /// The caller's `Content-Type`, or empty when it sent none.
    pub content_type: Vec<u8>,
    /// The relay ticket accompanying this call, or empty when none was sent
    /// (only the secret delivery itself travels without one).
    pub ticket: Vec<u8>,
    pub body: Vec<u8>,
}

/// One fragment of a request, borrowed from the payload it was decoded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestFragment<'a> {
    pub id: u32,
    pub method: RpcMethod,
    pub target: &'a [u8],
    pub seq: u16,
    pub total: u16,
    pub chunk: &'a [u8],
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
    /// The fragment's own index/total pair is impossible. Kept separate from
    /// [`Self::LengthMismatch`], whose fields are byte counts.
    BadFragmentIndex { index: u16, total: u16 },
    /// A response fragment claims a sender id longer than [`MAX_DEVICE_ID`].
    BadSenderLen(u8),
}

/// Why a request could not be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestEncodeError {
    /// The body is larger than [`MAX_REQUEST_BODY`]; the caller answers 413.
    BodyTooLarge,
    /// The target, path, content type or ticket cannot be length-prefixed.
    FieldTooLong,
}

/// Encode a request as the aux payloads (the bytes that go inside each aux
/// frame AFTER the 6-byte aux header) that carry it, in send order.
///
/// `target` is the device id the request is addressed to; an empty slice is a
/// broadcast every linked drone answers.
pub fn encode_request(
    method: RpcMethod,
    id: u32,
    target: &[u8],
    request: &RequestParts<'_>,
) -> Result<Vec<Vec<u8>>, RequestEncodeError> {
    if request.body.len() > MAX_REQUEST_BODY {
        return Err(RequestEncodeError::BodyTooLarge);
    }
    if target.len() > MAX_DEVICE_ID
        || request.path.len() > u16::MAX as usize
        || request.content_type.len() > u8::MAX as usize
        || request.ticket.len() > MAX_REQUEST_TICKET
    {
        return Err(RequestEncodeError::FieldTooLong);
    }
    let mut object = Vec::with_capacity(
        5 + request.path.len()
            + request.content_type.len()
            + request.ticket.len()
            + request.body.len(),
    );
    object.extend_from_slice(&(request.path.len() as u16).to_be_bytes());
    object.extend_from_slice(request.path);
    object.push(request.content_type.len() as u8);
    object.extend_from_slice(request.content_type);
    object.extend_from_slice(&(request.ticket.len() as u16).to_be_bytes());
    object.extend_from_slice(request.ticket);
    object.extend_from_slice(request.body);

    let chunks: Vec<&[u8]> = object.chunks(MAX_REQUEST_CHUNK).collect();
    let total = chunks.len() as u16;
    Ok(chunks
        .iter()
        .enumerate()
        .map(|(seq, chunk)| {
            let mut out =
                Vec::with_capacity(RPC_REQUEST_FRAGMENT_OVERHEAD + target.len() + chunk.len());
            out.push(method as u8);
            out.extend_from_slice(&id.to_be_bytes());
            out.push(target.len() as u8);
            out.extend_from_slice(target);
            out.extend_from_slice(&(seq as u16).to_be_bytes());
            out.extend_from_slice(&total.to_be_bytes());
            out.extend_from_slice(chunk);
            out
        })
        .collect())
}

/// The parts of a request that travel inside its object.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestParts<'a> {
    pub path: &'a [u8],
    pub content_type: &'a [u8],
    pub ticket: &'a [u8],
    pub body: &'a [u8],
}

/// Decode one request fragment.
pub fn decode_request_fragment(payload: &[u8]) -> Result<RequestFragment<'_>, RpcCodecError> {
    if payload.len() < RPC_REQUEST_FRAGMENT_OVERHEAD {
        return Err(RpcCodecError::TooShort);
    }
    let method = RpcMethod::from_u8(payload[0]).ok_or(RpcCodecError::BadMethod(payload[0]))?;
    let id = u32::from_be_bytes([payload[1], payload[2], payload[3], payload[4]]);
    let target_len = payload[5] as usize;
    let seq_offset = 6 + target_len;
    if seq_offset + 4 > payload.len() {
        return Err(RpcCodecError::LengthMismatch {
            declared: seq_offset + 4,
            actual: payload.len(),
        });
    }
    let target = &payload[6..seq_offset];
    let seq = u16::from_be_bytes([payload[seq_offset], payload[seq_offset + 1]]);
    let total = u16::from_be_bytes([payload[seq_offset + 2], payload[seq_offset + 3]]);
    if total == 0 || seq >= total || total as usize > MAX_REQUEST_FRAGMENTS {
        return Err(RpcCodecError::BadFragmentIndex { index: seq, total });
    }
    Ok(RequestFragment {
        id,
        method,
        target,
        seq,
        total,
        chunk: &payload[seq_offset + 4..],
    })
}

/// Split a reassembled request object into its fields.
fn decode_request_object(
    id: u32,
    method: RpcMethod,
    target: &[u8],
    object: &[u8],
) -> Result<RpcRequest, RpcCodecError> {
    let mut at = 0usize;
    let take = |at: &mut usize, n: usize| -> Result<std::ops::Range<usize>, RpcCodecError> {
        let end = at.checked_add(n).filter(|end| *end <= object.len()).ok_or(
            RpcCodecError::LengthMismatch {
                declared: at.saturating_add(n),
                actual: object.len(),
            },
        )?;
        let range = *at..end;
        *at = end;
        Ok(range)
    };
    let r = take(&mut at, 2)?;
    let path_len = u16::from_be_bytes([object[r.start], object[r.start + 1]]) as usize;
    let path = take(&mut at, path_len)?;
    let r = take(&mut at, 1)?;
    let ct_len = object[r.start] as usize;
    let content_type = take(&mut at, ct_len)?;
    let r = take(&mut at, 2)?;
    let ticket_len = u16::from_be_bytes([object[r.start], object[r.start + 1]]) as usize;
    if ticket_len > MAX_REQUEST_TICKET {
        return Err(RpcCodecError::LengthMismatch {
            declared: ticket_len,
            actual: MAX_REQUEST_TICKET,
        });
    }
    let ticket = take(&mut at, ticket_len)?;
    let body = &object[at..];
    if body.len() > MAX_REQUEST_BODY {
        return Err(RpcCodecError::LengthMismatch {
            declared: body.len(),
            actual: MAX_REQUEST_BODY,
        });
    }
    Ok(RpcRequest {
        id,
        method,
        target: target.to_vec(),
        path: object[path].to_vec(),
        content_type: object[content_type].to_vec(),
        ticket: object[ticket].to_vec(),
        body: body.to_vec(),
    })
}

/// What one pushed fragment settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReassemblyOutcome {
    /// Every fragment is in and the object decoded.
    Complete(RpcRequest),
    /// More fragments are needed.
    Pending,
    /// Already held, or a fragment of a request this reassembler dropped.
    Duplicate,
    /// Dropped: the request would exceed a buffering bound, or its object did
    /// not decode.
    Rejected,
}

struct PartialRequest {
    method: RpcMethod,
    target: Vec<u8>,
    total: u16,
    chunks: Vec<Option<Vec<u8>>>,
    received: u16,
    bytes: usize,
    started: std::time::Instant,
}

/// Collects request fragments on the drone until a request is whole.
///
/// Bounded in count, bytes and time: the uplink is a broadcast any fleet member
/// can transmit on, and nothing in a fragment is authenticated until the whole
/// object (and the ticket inside it) has arrived.
#[derive(Default)]
pub struct RequestReassembler {
    partials: std::collections::HashMap<u32, PartialRequest>,
    buffered: usize,
}

impl RequestReassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one fragment received at `now`.
    pub fn push(
        &mut self,
        fragment: &RequestFragment<'_>,
        now: std::time::Instant,
    ) -> ReassemblyOutcome {
        self.expire(now);
        if fragment.total == 1 {
            return match decode_request_object(
                fragment.id,
                fragment.method,
                fragment.target,
                fragment.chunk,
            ) {
                Ok(request) => ReassemblyOutcome::Complete(request),
                Err(_) => ReassemblyOutcome::Rejected,
            };
        }
        if fragment.chunk.len() > MAX_REQUEST_CHUNK {
            return ReassemblyOutcome::Rejected;
        }
        // A fragment that disagrees with the partial already held under its id
        // is a different request reusing the id (a restarted ground station);
        // the newer one wins.
        let conflicting = self.partials.get(&fragment.id).is_some_and(|p| {
            p.method != fragment.method || p.total != fragment.total || p.target != fragment.target
        });
        if conflicting {
            self.drop_partial(fragment.id);
        }
        if !self.partials.contains_key(&fragment.id) {
            if self.partials.len() >= MAX_PARTIAL_REQUESTS {
                return ReassemblyOutcome::Rejected;
            }
            self.partials.insert(
                fragment.id,
                PartialRequest {
                    method: fragment.method,
                    target: fragment.target.to_vec(),
                    total: fragment.total,
                    chunks: vec![None; fragment.total as usize],
                    received: 0,
                    bytes: 0,
                    started: now,
                },
            );
        }
        let buffered = self.buffered;
        let Some(partial) = self.partials.get_mut(&fragment.id) else {
            return ReassemblyOutcome::Rejected;
        };
        if partial.chunks[fragment.seq as usize].is_some() {
            return ReassemblyOutcome::Duplicate;
        }
        if buffered + fragment.chunk.len() > MAX_PARTIAL_REQUEST_BYTES
            || partial.bytes + fragment.chunk.len() > MAX_REQUEST_OBJECT
        {
            self.drop_partial(fragment.id);
            return ReassemblyOutcome::Rejected;
        }
        partial.chunks[fragment.seq as usize] = Some(fragment.chunk.to_vec());
        partial.received += 1;
        partial.bytes += fragment.chunk.len();
        self.buffered += fragment.chunk.len();
        if partial.received < partial.total {
            return ReassemblyOutcome::Pending;
        }
        let Some(done) = self.partials.remove(&fragment.id) else {
            return ReassemblyOutcome::Rejected;
        };
        self.buffered = self.buffered.saturating_sub(done.bytes);
        let mut object = Vec::with_capacity(done.bytes);
        for chunk in done.chunks.into_iter().flatten() {
            object.extend_from_slice(&chunk);
        }
        match decode_request_object(fragment.id, done.method, &done.target, &object) {
            Ok(request) => ReassemblyOutcome::Complete(request),
            Err(_) => ReassemblyOutcome::Rejected,
        }
    }

    /// Requests currently partially held.
    pub fn partial_count(&self) -> usize {
        self.partials.len()
    }

    fn drop_partial(&mut self, id: u32) {
        if let Some(p) = self.partials.remove(&id) {
            self.buffered = self.buffered.saturating_sub(p.bytes);
        }
    }

    fn expire(&mut self, now: std::time::Instant) {
        let stale: Vec<u32> = self
            .partials
            .iter()
            .filter(|(_, p)| now.saturating_duration_since(p.started) > REQUEST_REASSEMBLY_TIMEOUT)
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            self.drop_partial(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn parts<'a>(path: &'a [u8], body: &'a [u8]) -> RequestParts<'a> {
        RequestParts {
            path,
            content_type: b"",
            ticket: b"",
            body,
        }
    }

    /// Encode and reassemble in order, the happy path a radio delivers.
    fn round_trip(method: RpcMethod, id: u32, target: &[u8], req: &RequestParts<'_>) -> RpcRequest {
        let frags = encode_request(method, id, target, req).unwrap();
        let mut r = RequestReassembler::new();
        let now = Instant::now();
        let mut done = None;
        for f in &frags {
            if let ReassemblyOutcome::Complete(req) =
                r.push(&decode_request_fragment(f).unwrap(), now)
            {
                done = Some(req);
            }
        }
        done.expect("the request completes")
    }

    #[test]
    fn a_small_request_is_one_fragment_and_round_trips() {
        let req = RequestParts {
            path: b"/api/config",
            content_type: b"application/json",
            ticket: b"v2|tok",
            body: br#"{"key":"agent.name","value":"example-drone"}"#,
        };
        let frags = encode_request(RpcMethod::Put, 7, b"0a1b2c3d4e5f", &req).unwrap();
        assert_eq!(frags.len(), 1);
        assert!(frags[0].len() <= AUX_MAX_PAYLOAD);
        let got = round_trip(RpcMethod::Put, 7, b"0a1b2c3d4e5f", &req);
        assert_eq!(got.id, 7);
        assert_eq!(got.method, RpcMethod::Put);
        assert_eq!(got.target, b"0a1b2c3d4e5f");
        assert_eq!(got.path, req.path);
        assert_eq!(got.content_type, req.content_type);
        assert_eq!(got.ticket, req.ticket);
        assert_eq!(got.body, req.body);
    }

    #[test]
    fn a_body_far_past_one_frame_fragments_and_reassembles_out_of_order() {
        // Plugin uploads and larger writes used to be refused with a 413.
        let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let req = RequestParts {
            path: b"/api/plugins/com.example.tool/x/upload",
            content_type: b"application/octet-stream",
            ticket: b"",
            body: &body,
        };
        let frags = encode_request(RpcMethod::Patch, 9, b"d1", &req).unwrap();
        assert!(frags.len() > 100);
        assert!(frags.iter().all(|f| f.len() <= AUX_MAX_PAYLOAD));
        let mut r = RequestReassembler::new();
        let now = Instant::now();
        let mut order: Vec<usize> = (0..frags.len()).rev().collect();
        // A duplicate in the middle of the stream must not count twice.
        order.insert(5, order[3]);
        let mut done = None;
        for i in order {
            match r.push(&decode_request_fragment(&frags[i]).unwrap(), now) {
                ReassemblyOutcome::Complete(req) => done = Some(req),
                ReassemblyOutcome::Pending | ReassemblyOutcome::Duplicate => {}
                ReassemblyOutcome::Rejected => panic!("rejected"),
            }
        }
        let got = done.expect("complete");
        assert_eq!(got.method, RpcMethod::Patch);
        assert_eq!(got.body, body);
        assert_eq!(got.content_type, b"application/octet-stream");
        assert_eq!(r.partial_count(), 0);
    }

    #[test]
    fn a_body_over_one_mebibyte_is_refused_at_encode() {
        let body = vec![0u8; MAX_REQUEST_BODY + 1];
        assert_eq!(
            encode_request(RpcMethod::Post, 1, b"d1", &parts(b"/api/x", &body)),
            Err(RequestEncodeError::BodyTooLarge)
        );
        let body = vec![0u8; MAX_REQUEST_BODY];
        assert!(encode_request(RpcMethod::Post, 1, b"d1", &parts(b"/api/x", &body)).is_ok());
    }

    #[test]
    fn a_partial_request_is_dropped_after_the_reassembly_window() {
        let body = vec![1u8; 5_000];
        let frags = encode_request(RpcMethod::Post, 3, b"d1", &parts(b"/api/x", &body)).unwrap();
        let mut r = RequestReassembler::new();
        let t0 = Instant::now();
        assert_eq!(
            r.push(&decode_request_fragment(&frags[0]).unwrap(), t0),
            ReassemblyOutcome::Pending
        );
        let later = t0 + REQUEST_REASSEMBLY_TIMEOUT + Duration::from_secs(1);
        for f in &frags[1..] {
            assert_eq!(
                r.push(&decode_request_fragment(f).unwrap(), later),
                ReassemblyOutcome::Pending,
                "the stale first fragment is gone, so this cannot complete"
            );
        }
    }

    #[test]
    fn concurrent_partials_are_bounded() {
        let body = vec![1u8; 3_000];
        let mut r = RequestReassembler::new();
        let now = Instant::now();
        for id in 0..MAX_PARTIAL_REQUESTS as u32 {
            let frags =
                encode_request(RpcMethod::Post, id, b"d1", &parts(b"/api/x", &body)).unwrap();
            assert_eq!(
                r.push(&decode_request_fragment(&frags[0]).unwrap(), now),
                ReassemblyOutcome::Pending
            );
        }
        let frags = encode_request(RpcMethod::Post, 999, b"d1", &parts(b"/api/x", &body)).unwrap();
        assert_eq!(
            r.push(&decode_request_fragment(&frags[0]).unwrap(), now),
            ReassemblyOutcome::Rejected
        );
    }

    #[test]
    fn rejects_a_target_longer_than_a_device_id() {
        let too_long = vec![b'a'; MAX_DEVICE_ID + 1];
        assert_eq!(
            encode_request(RpcMethod::Get, 1, &too_long, &parts(b"/api/x", b"")),
            Err(RequestEncodeError::FieldTooLong)
        );
        let fits = vec![b'a'; MAX_DEVICE_ID];
        assert_eq!(
            round_trip(RpcMethod::Get, 1, &fits, &parts(b"/api/x", b"")).target,
            fits
        );
    }

    #[test]
    fn rejects_a_truncated_fragment() {
        let frags =
            encode_request(RpcMethod::Post, 9, b"abc", &parts(b"/api/x", b"payload")).unwrap();
        assert_eq!(
            decode_request_fragment(&frags[0][..5]).unwrap_err(),
            RpcCodecError::TooShort
        );
    }

    #[test]
    fn an_impossible_sequence_number_is_refused() {
        let mut frag =
            encode_request(RpcMethod::Get, 1, b"", &parts(b"/api/x", b"")).unwrap()[0].clone();
        // seq (bytes 6..8 with an empty target) set past total.
        frag[6..8].copy_from_slice(&3u16.to_be_bytes());
        assert_eq!(
            decode_request_fragment(&frag).unwrap_err(),
            RpcCodecError::BadFragmentIndex { index: 3, total: 1 }
        );
    }

    #[test]
    fn an_object_whose_lengths_lie_is_rejected() {
        let mut frag =
            encode_request(RpcMethod::Get, 1, b"", &parts(b"/api/x", b"")).unwrap()[0].clone();
        // path_len (first two chunk bytes) claims more than is present.
        frag[10..12].copy_from_slice(&500u16.to_be_bytes());
        let mut r = RequestReassembler::new();
        assert_eq!(
            r.push(&decode_request_fragment(&frag).unwrap(), Instant::now()),
            ReassemblyOutcome::Rejected
        );
    }

    #[test]
    fn rejects_an_unknown_method_byte() {
        let mut frag =
            encode_request(RpcMethod::Get, 1, b"", &parts(b"/api/x", b"")).unwrap()[0].clone();
        frag[0] = 0xFF;
        assert_eq!(
            decode_request_fragment(&frag).unwrap_err(),
            RpcCodecError::BadMethod(0xFF)
        );
    }

    #[test]
    fn method_numbers_are_pinned() {
        // Renumbering would silently reroute a POST into a GET handler.
        assert_eq!(RpcMethod::Get as u8, 1);
        assert_eq!(RpcMethod::Post as u8, 2);
        assert_eq!(RpcMethod::Put as u8, 3);
        assert_eq!(RpcMethod::Delete as u8, 4);
        assert_eq!(RpcMethod::Patch as u8, 5);
        assert_eq!(RpcMethod::from_u8(5), Some(RpcMethod::Patch));
        assert_eq!(RpcMethod::from_u8(0), None);
        for m in [
            RpcMethod::Get,
            RpcMethod::Post,
            RpcMethod::Put,
            RpcMethod::Delete,
            RpcMethod::Patch,
        ] {
            assert_eq!(RpcMethod::from_http_method(m.as_http_method()), Some(m));
        }
        assert_eq!(RpcMethod::from_http_method("OPTIONS"), None);
    }
}
