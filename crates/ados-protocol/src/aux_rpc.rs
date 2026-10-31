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
//! The MAVLink TUNNEL extension (the `ados-mavlink-tunnel` crate) chunks a single body
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

mod reassembly;
mod request;
pub use reassembly::*;
pub use request::*;

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

#[cfg(test)]
mod tests;
