//! L3 config-over-radio intra-payload framing: chunking + reassembly.
//!
//! The MAVLink `TUNNEL` frame (message id 385) is a transparent opaque pipe
//! that carries at most [`crate::mavlink::TUNNEL_MAX_PAYLOAD`] (128) bytes per
//! frame and gives no delivery guarantee. A config request/response body (a
//! JSON envelope, sometimes a few hundred bytes) does not fit one frame, so
//! this module defines the tight binary header that lets a body span several
//! TUNNEL frames and be reassembled on the far side.
//!
//! ## Why a bespoke header (not the plugin [`crate::plugin::Envelope`])
//!
//! The plugin RPC envelope is msgpack with a 4-byte length prefix; it does not
//! fit the 128-byte tunnel budget once a chunk header is added. This header is
//! a fixed 12 bytes so ~116 bytes of body ride each frame. The correlation
//! `request_id` semantics are borrowed from the envelope (the responder echoes
//! the request's id) so both agent halves agree, but the on-wire encoding is
//! this module's own.
//!
//! ## Wire layout of one chunk payload (rides inside a TUNNEL payload)
//!
//! ```text
//!   byte 0..2   magic   = [0xAD, 0x03]        (distinguishes L3 frames from
//!                                              unrelated app traffic sharing
//!                                              the private payload_type)
//!   byte 2      version = 0x01                (this header format)
//!   byte 3      flags   bit0 = is_response, bit1 = is_error, rest reserved 0
//!   byte 4..8   request_id  (u32, little-endian) — correlation id
//!   byte 8..10  seq         (u16, little-endian) — this chunk index, 0-based
//!   byte 10..12 total       (u16, little-endian) — total chunk count
//!   byte 12..   data        — up to MAX_CHUNK_DATA bytes of the body slice
//! ```
//!
//! A whole message body is the concatenation of the chunk data slices in
//! `seq` order, `0..total`.
//!
//! ## What this module is NOT
//!
//! It carries no application semantics and no MAVLink dependency — it is pure
//! binary framing plus a bounded reassembler. Building the outer TUNNEL frame
//! ([`crate::mavlink::build_tunnel_v2`]) and moving it over a bearer are the
//! caller's job. The config request/response semantics, the localhost
//! `/api/config` proxy, and the `-p1` safety gate live in the config-tunnel
//! service, not here.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Private application `payload_type` for the L3 config channel, tagging every
/// TUNNEL frame this substrate mints. It is in the private range
/// (`> crate::mavlink::TUNNEL_RESERVED_PAYLOAD_TYPE_MAX`, i.e. `32768..=65535`)
/// so it can never collide with a registered type and an unrelated peer that
/// does not recognise it ignores the frame. A single type carries both
/// directions; the request/response distinction is the `is_response` flag in
/// the chunk header, so a demux needs one `tunnel_payload_type` compare.
pub const CONFIG_TUNNEL_PAYLOAD_TYPE: u16 = 0xADC0;

/// Chunk-header magic — the first two payload bytes. A plugin holding the
/// `mavlink.tunnel` capability can mint a TUNNEL with any private payload_type,
/// so the reassembler double-checks this magic (plus [`L3_VERSION`]) and
/// ignores any payload that does not carry it, never mistaking unrelated app
/// traffic on the same type for a config chunk.
pub const L3_MAGIC: [u8; 2] = [0xAD, 0x03];

/// Chunk-header format version. Bump on an incompatible header change; the
/// parser rejects an unrecognised version rather than misreading it.
pub const L3_VERSION: u8 = 1;

/// Fixed chunk-header length in bytes (magic 2 + version 1 + flags 1 +
/// request_id 4 + seq 2 + total 2).
pub const L3_HEADER_LEN: usize = 12;

/// The TUNNEL single-frame payload cap this module chunks under. Mirrors
/// [`crate::mavlink::TUNNEL_MAX_PAYLOAD`]; a `#[cfg(feature = "mavlink")]` test
/// asserts they agree so a drift is caught.
pub const TUNNEL_PAYLOAD_CAP: usize = 128;

/// Largest body slice one chunk can carry (the payload cap minus the header).
pub const MAX_CHUNK_DATA: usize = TUNNEL_PAYLOAD_CAP - L3_HEADER_LEN;

// Compile-time invariants: the header + data cap exactly fill the tunnel
// payload budget, and the payload_type is in the private application range.
const _: () = assert!(L3_HEADER_LEN + MAX_CHUNK_DATA == TUNNEL_PAYLOAD_CAP);
const _: () = assert!(CONFIG_TUNNEL_PAYLOAD_TYPE > 32767);
#[cfg(feature = "mavlink")]
const _: () = assert!(TUNNEL_PAYLOAD_CAP == crate::mavlink::TUNNEL_MAX_PAYLOAD);
#[cfg(feature = "mavlink")]
const _: () =
    assert!(CONFIG_TUNNEL_PAYLOAD_TYPE > crate::mavlink::TUNNEL_RESERVED_PAYLOAD_TYPE_MAX);

const FLAG_IS_RESPONSE: u8 = 0b0000_0001;
const FLAG_IS_ERROR: u8 = 0b0000_0010;

/// A build/parse failure with a stable, human-readable message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TunnelConfigError {
    /// A chunk's data slice exceeds [`MAX_CHUNK_DATA`].
    #[error("chunk data is {got} bytes, exceeds the {MAX_CHUNK_DATA}-byte chunk limit")]
    ChunkTooLarge { got: usize },
    /// `total` was zero, or `seq >= total` — an impossible chunk index.
    #[error("chunk index seq={seq} is out of range for total={total}")]
    BadChunkIndex { seq: u16, total: u16 },
    /// A body would split into more chunks than the caller's ceiling allows.
    #[error("body needs {needed} chunks, exceeds the {limit}-chunk limit")]
    TooManyChunks { needed: usize, limit: usize },
}

/// The parsed fields of one chunk header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkHeader {
    /// Correlation id echoed request↔response so the far side matches a reply
    /// to its request over the shared broadcast bearer.
    pub request_id: u32,
    /// This chunk's 0-based index within the message.
    pub seq: u16,
    /// Total number of chunks the message spans.
    pub total: u16,
    /// True on a response frame (drone→GS), false on a request (GS→drone).
    pub is_response: bool,
    /// True when the (response) body is an error envelope rather than a
    /// successful config result.
    pub is_error: bool,
}

impl ChunkHeader {
    fn flags(&self) -> u8 {
        let mut f = 0u8;
        if self.is_response {
            f |= FLAG_IS_RESPONSE;
        }
        if self.is_error {
            f |= FLAG_IS_ERROR;
        }
        f
    }
}

/// Serialize one chunk payload (`header || data`), ready to hand to
/// [`crate::mavlink::build_tunnel_v2`] as the TUNNEL payload.
///
/// Rejects a `data` slice longer than [`MAX_CHUNK_DATA`] and an impossible
/// `(seq, total)` (`total == 0` or `seq >= total`).
pub fn build_chunk(header: &ChunkHeader, data: &[u8]) -> Result<Vec<u8>, TunnelConfigError> {
    if data.len() > MAX_CHUNK_DATA {
        return Err(TunnelConfigError::ChunkTooLarge { got: data.len() });
    }
    if header.total == 0 || header.seq >= header.total {
        return Err(TunnelConfigError::BadChunkIndex {
            seq: header.seq,
            total: header.total,
        });
    }
    let mut out = Vec::with_capacity(L3_HEADER_LEN + data.len());
    out.extend_from_slice(&L3_MAGIC);
    out.push(L3_VERSION);
    out.push(header.flags());
    out.extend_from_slice(&header.request_id.to_le_bytes());
    out.extend_from_slice(&header.seq.to_le_bytes());
    out.extend_from_slice(&header.total.to_le_bytes());
    out.extend_from_slice(data);
    Ok(out)
}

/// Parse one chunk payload, returning the header and a borrow of the data
/// slice. Returns `None` — never an error — for any payload that is not a
/// well-formed L3 chunk (too short, wrong magic, wrong version, or an
/// impossible `(seq, total)`), so a demux can ignore non-conforming traffic
/// sharing the private payload_type without a decision to make.
#[must_use]
pub fn parse_chunk(payload: &[u8]) -> Option<(ChunkHeader, &[u8])> {
    if payload.len() < L3_HEADER_LEN {
        return None;
    }
    if payload[0..2] != L3_MAGIC || payload[2] != L3_VERSION {
        return None;
    }
    let flags = payload[3];
    let request_id = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let seq = u16::from_le_bytes([payload[8], payload[9]]);
    let total = u16::from_le_bytes([payload[10], payload[11]]);
    if total == 0 || seq >= total {
        return None;
    }
    let header = ChunkHeader {
        request_id,
        seq,
        total,
        is_response: flags & FLAG_IS_RESPONSE != 0,
        is_error: flags & FLAG_IS_ERROR != 0,
    };
    Some((header, &payload[L3_HEADER_LEN..]))
}

/// Split a message body into ordered chunk payloads, each ready for a TUNNEL
/// frame. An empty body yields a single zero-length chunk (`total = 1`) so the
/// far side still sees exactly one message. Rejects a body that would exceed
/// `max_chunks` frames.
pub fn chunk_message(
    request_id: u32,
    is_response: bool,
    is_error: bool,
    body: &[u8],
    max_chunks: usize,
) -> Result<Vec<Vec<u8>>, TunnelConfigError> {
    // ceil(len / MAX_CHUNK_DATA), but at least one chunk (an empty body still
    // travels as one zero-length chunk).
    let needed = if body.is_empty() {
        1
    } else {
        body.len().div_ceil(MAX_CHUNK_DATA)
    };
    if needed > max_chunks || needed > u16::MAX as usize {
        return Err(TunnelConfigError::TooManyChunks {
            needed,
            limit: max_chunks.min(u16::MAX as usize),
        });
    }
    let total = needed as u16;
    let mut chunks = Vec::with_capacity(needed);
    for seq in 0..needed {
        let start = seq * MAX_CHUNK_DATA;
        let end = (start + MAX_CHUNK_DATA).min(body.len());
        let slice = if body.is_empty() {
            &[][..]
        } else {
            &body[start..end]
        };
        let header = ChunkHeader {
            request_id,
            seq: seq as u16,
            total,
            is_response,
            is_error,
        };
        chunks.push(build_chunk(&header, slice)?);
    }
    Ok(chunks)
}

/// A completed, reassembled message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedMessage {
    pub request_id: u32,
    pub is_response: bool,
    pub is_error: bool,
    /// The reassembled body (the concatenation of the chunk data slices).
    pub body: Vec<u8>,
}

/// What [`Reassembler::push`] did with one chunk payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    /// The payload was not a conforming L3 chunk (wrong magic/version/shape),
    /// or contradicted an in-flight message it could not join — dropped, no
    /// state change worth acting on.
    Ignored,
    /// A valid chunk accepted; the message is not yet complete.
    Incomplete,
    /// The final missing chunk arrived; the whole message is returned and its
    /// reassembly state is freed.
    Complete(CompletedMessage),
    /// The chunk was well-formed but breached a bound (too many chunks, or the
    /// accumulated body exceeded the cap); the whole in-flight message is
    /// dropped and freed. The string names why (for a truthful log/counter).
    Rejected(String),
}

/// One in-flight message's partial state.
#[derive(Debug)]
struct Partial {
    is_response: bool,
    is_error: bool,
    total: u16,
    chunks: Vec<Option<Vec<u8>>>,
    received: usize,
    bytes: usize,
    created: Instant,
}

/// A bounded, timeout-swept chunk reassembler keyed by `request_id`.
///
/// It is a shared receiver for the broadcast bearer: several messages can be
/// in flight at once, out-of-order and lossy. Every message is bounded — a
/// `total` above `max_chunks` or an accumulated body above `max_body_bytes` is
/// rejected and freed, and [`Reassembler::sweep`] drops any message that has
/// not completed within a caller-supplied deadline — so a peer flooding
/// partial messages can never grow memory without bound.
#[derive(Debug)]
pub struct Reassembler {
    pending: HashMap<u32, Partial>,
    max_chunks: usize,
    max_body_bytes: usize,
}

impl Reassembler {
    /// Create a reassembler with the given per-message ceilings.
    #[must_use]
    pub fn new(max_chunks: usize, max_body_bytes: usize) -> Self {
        Self {
            pending: HashMap::new(),
            max_chunks,
            max_body_bytes,
        }
    }

    /// Number of in-flight (incomplete) messages, for introspection/telemetry.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.pending.len()
    }

    /// Feed one chunk payload (the inner TUNNEL payload, not the whole frame)
    /// with the current clock reading. See [`PushOutcome`].
    pub fn push(&mut self, payload: &[u8], now: Instant) -> PushOutcome {
        let Some((header, data)) = parse_chunk(payload) else {
            return PushOutcome::Ignored;
        };
        if header.total as usize > self.max_chunks {
            // Never allocate a chunk vector for an over-cap message; also drop
            // any prior in-flight state under this id.
            self.pending.remove(&header.request_id);
            return PushOutcome::Rejected(format!(
                "total {} exceeds max_chunks {}",
                header.total, self.max_chunks
            ));
        }

        // Get the in-flight message under this id, replacing a stale one whose
        // shape (total/direction/error) disagrees — that is a different
        // message reusing the id after the old one never completed.
        let fresh = match self.pending.get(&header.request_id) {
            Some(p) => {
                p.total != header.total
                    || p.is_response != header.is_response
                    || p.is_error != header.is_error
            }
            None => true,
        };
        if fresh {
            self.pending.insert(
                header.request_id,
                Partial {
                    is_response: header.is_response,
                    is_error: header.is_error,
                    total: header.total,
                    chunks: vec![None; header.total as usize],
                    received: 0,
                    bytes: 0,
                    created: now,
                },
            );
        }
        let partial = self
            .pending
            .get_mut(&header.request_id)
            .expect("just inserted or already present");

        // Place (or overwrite a duplicate) this chunk, keeping the byte count
        // exact.
        let slot = &mut partial.chunks[header.seq as usize];
        if let Some(old) = slot.take() {
            partial.bytes -= old.len();
            partial.received -= 1;
        }
        partial.bytes += data.len();
        partial.received += 1;
        *slot = Some(data.to_vec());

        if partial.bytes > self.max_body_bytes {
            let bytes = partial.bytes;
            let cap = self.max_body_bytes;
            self.pending.remove(&header.request_id);
            return PushOutcome::Rejected(format!("body {bytes} bytes exceeds cap {cap}"));
        }

        if partial.received < partial.total as usize {
            return PushOutcome::Incomplete;
        }

        // Complete: assemble in seq order and free the entry.
        let partial = self
            .pending
            .remove(&header.request_id)
            .expect("present above");
        let mut body = Vec::with_capacity(partial.bytes);
        for chunk in partial.chunks {
            body.extend_from_slice(&chunk.expect("received == total ⇒ every slot filled"));
        }
        PushOutcome::Complete(CompletedMessage {
            request_id: header.request_id,
            is_response: partial.is_response,
            is_error: partial.is_error,
            body,
        })
    }

    /// Drop every in-flight message whose first chunk arrived more than
    /// `timeout` before `now`. Returns the number dropped so a caller can
    /// surface a truthful "N reassembly timeouts" counter.
    pub fn sweep(&mut self, now: Instant, timeout: Duration) -> usize {
        let before = self.pending.len();
        self.pending
            .retain(|_, p| now.duration_since(p.created) <= timeout);
        before - self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(request_id: u32, seq: u16, total: u16) -> ChunkHeader {
        ChunkHeader {
            request_id,
            seq,
            total,
            is_response: false,
            is_error: false,
        }
    }

    #[test]
    fn header_len_and_data_cap_fit_the_tunnel_budget() {
        // The compile-time `const _` assertions above pin the budget/range
        // invariants; this pins the concrete data cap the wire layout implies.
        assert_eq!(L3_HEADER_LEN + MAX_CHUNK_DATA, TUNNEL_PAYLOAD_CAP);
        assert_eq!(MAX_CHUNK_DATA, 116);
    }

    #[test]
    fn build_then_parse_round_trips_header_and_data() {
        let h = ChunkHeader {
            request_id: 0xDEAD_BEEF,
            seq: 3,
            total: 9,
            is_response: true,
            is_error: true,
        };
        let data = b"the quick brown fox";
        let payload = build_chunk(&h, data).unwrap();
        assert_eq!(payload.len(), L3_HEADER_LEN + data.len());
        let (got, got_data) = parse_chunk(&payload).unwrap();
        assert_eq!(got, h);
        assert_eq!(got_data, data);
    }

    #[test]
    fn build_rejects_oversize_data_and_bad_index() {
        let big = vec![0u8; MAX_CHUNK_DATA + 1];
        assert_eq!(
            build_chunk(&hdr(1, 0, 1), &big),
            Err(TunnelConfigError::ChunkTooLarge {
                got: MAX_CHUNK_DATA + 1
            })
        );
        assert!(matches!(
            build_chunk(&hdr(1, 2, 2), b"x"),
            Err(TunnelConfigError::BadChunkIndex { seq: 2, total: 2 })
        ));
        assert!(matches!(
            build_chunk(&hdr(1, 0, 0), b"x"),
            Err(TunnelConfigError::BadChunkIndex { seq: 0, total: 0 })
        ));
    }

    #[test]
    fn parse_ignores_non_conforming_payloads() {
        assert!(parse_chunk(b"").is_none());
        assert!(parse_chunk(&[0u8; L3_HEADER_LEN - 1]).is_none());
        // Right length, wrong magic.
        let mut bad = vec![0xFFu8; L3_HEADER_LEN];
        bad[10] = 1; // total=1
        assert!(parse_chunk(&bad).is_none());
        // Right magic, wrong version.
        let mut wrong_ver = build_chunk(&hdr(7, 0, 1), b"hi").unwrap();
        wrong_ver[2] = L3_VERSION + 1;
        assert!(parse_chunk(&wrong_ver).is_none());
        // Right magic+version, impossible index (seq >= total).
        let mut bad_idx = build_chunk(&hdr(7, 0, 2), b"hi").unwrap();
        bad_idx[8] = 5; // seq=5, total=2
        assert!(parse_chunk(&bad_idx).is_none());
    }

    #[test]
    fn chunk_message_splits_and_reassembles_in_order() {
        let body: Vec<u8> = (0..(MAX_CHUNK_DATA * 3 + 7)).map(|i| i as u8).collect();
        let chunks = chunk_message(42, true, false, &body, 512).unwrap();
        assert_eq!(chunks.len(), 4);
        let mut re = Reassembler::new(512, 1 << 20);
        let now = Instant::now();
        let mut done = None;
        for c in &chunks {
            match re.push(c, now) {
                PushOutcome::Incomplete => {}
                PushOutcome::Complete(m) => done = Some(m),
                other => panic!("unexpected {other:?}"),
            }
        }
        let m = done.expect("completed");
        assert_eq!(m.request_id, 42);
        assert!(m.is_response && !m.is_error);
        assert_eq!(m.body, body);
        assert_eq!(re.in_flight(), 0);
    }

    #[test]
    fn empty_body_travels_as_one_chunk() {
        let chunks = chunk_message(1, false, false, b"", 8).unwrap();
        assert_eq!(chunks.len(), 1);
        let mut re = Reassembler::new(8, 64);
        match re.push(&chunks[0], Instant::now()) {
            PushOutcome::Complete(m) => assert!(m.body.is_empty()),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn reassembles_out_of_order_and_tolerates_duplicates() {
        let body: Vec<u8> = (0..250u16).map(|i| i as u8).collect();
        let chunks = chunk_message(99, false, false, &body, 512).unwrap();
        assert_eq!(chunks.len(), 3);
        let mut re = Reassembler::new(512, 1 << 20);
        let now = Instant::now();
        // Out of order, with a duplicate of chunk 2 before chunk 0.
        assert_eq!(re.push(&chunks[2], now), PushOutcome::Incomplete);
        assert_eq!(re.push(&chunks[1], now), PushOutcome::Incomplete);
        assert_eq!(re.push(&chunks[2], now), PushOutcome::Incomplete); // dup, no double-count
        match re.push(&chunks[0], now) {
            PushOutcome::Complete(m) => assert_eq!(m.body, body),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_lost_chunk_never_completes_and_sweeps_out() {
        let body: Vec<u8> = vec![7u8; MAX_CHUNK_DATA * 2];
        let chunks = chunk_message(3, false, false, &body, 8).unwrap();
        let mut re = Reassembler::new(8, 1 << 20);
        let base = Instant::now();
        assert_eq!(re.push(&chunks[0], base), PushOutcome::Incomplete);
        // chunks[1] is "lost"; the message never completes.
        assert_eq!(re.in_flight(), 1);
        assert_eq!(
            re.sweep(base + Duration::from_secs(9), Duration::from_secs(10)),
            0
        );
        assert_eq!(
            re.sweep(base + Duration::from_secs(11), Duration::from_secs(10)),
            1
        );
        assert_eq!(re.in_flight(), 0);
    }

    #[test]
    fn rejects_over_cap_total_and_over_cap_body() {
        // total above max_chunks: rejected before allocating.
        let chunk = build_chunk(&hdr(5, 0, 100), b"x").unwrap();
        let mut re = Reassembler::new(8, 1 << 20);
        assert!(matches!(
            re.push(&chunk, Instant::now()),
            PushOutcome::Rejected(_)
        ));
        assert_eq!(re.in_flight(), 0);

        // Accumulated body above the byte cap: the whole message is freed.
        let body = vec![0u8; MAX_CHUNK_DATA * 3];
        let chunks = chunk_message(6, false, false, &body, 512).unwrap();
        let mut re = Reassembler::new(512, MAX_CHUNK_DATA * 2); // cap below the body
        let now = Instant::now();
        assert_eq!(re.push(&chunks[0], now), PushOutcome::Incomplete);
        assert_eq!(re.push(&chunks[1], now), PushOutcome::Incomplete);
        assert!(matches!(re.push(&chunks[2], now), PushOutcome::Rejected(_)));
        assert_eq!(re.in_flight(), 0);
    }

    #[test]
    fn chunk_message_rejects_a_body_over_the_chunk_limit() {
        let body = vec![0u8; MAX_CHUNK_DATA * 5];
        assert!(matches!(
            chunk_message(1, false, false, &body, 3),
            Err(TunnelConfigError::TooManyChunks {
                needed: 5,
                limit: 3
            })
        ));
    }

    #[test]
    fn a_reused_id_with_a_new_shape_resets_the_prior_message() {
        let mut re = Reassembler::new(512, 1 << 20);
        let now = Instant::now();
        // Start a 3-chunk message under id 8, deliver one chunk.
        let first = chunk_message(8, false, false, &vec![1u8; MAX_CHUNK_DATA * 3], 512).unwrap();
        assert_eq!(re.push(&first[0], now), PushOutcome::Incomplete);
        assert_eq!(re.in_flight(), 1);
        // A brand-new single-chunk message reuses id 8: it replaces the stale
        // one and completes on its own.
        let second = chunk_message(8, false, false, b"fresh", 512).unwrap();
        match re.push(&second[0], now) {
            PushOutcome::Complete(m) => assert_eq!(m.body, b"fresh"),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(re.in_flight(), 0);
    }
}
