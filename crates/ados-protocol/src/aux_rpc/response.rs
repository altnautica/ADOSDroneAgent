//! Response half of the relay-proxy RPC codec.
//!
//! A request fits one aux datagram; a response rarely does, so it is spread
//! across fragments by [`split_response`] and rebuilt on the ground by
//! [`ResponseDecoder`]. Two properties the request half does not need drive
//! everything here.
//!
//! ## Fragments are RaptorQ symbols, not slices
//!
//! Per-fragment downlink loss measures 0.7%. All-or-nothing reassembly of a
//! 26-fragment body therefore succeeds `(1 - 0.007)^26 = 83%`, which is the
//! 17/20 measured against a live drone on `/api/status/full`. The failure is
//! not a transport bug; it is geometric decay in the fragment count, and no
//! amount of retransmitting a whole 30 KB response fixes it cheaply.
//!
//! So a body is encoded as one RaptorQ source block: `k` systematic symbols
//! plus [`RPC_REPAIR_SYMBOLS`] repair symbols, of which **any `k`** rebuild the
//! body. At `k = 26` that is 15% overhead — less than the aux lane's existing
//! 2x FEC duplication — and drops the same failure below `1e-5`.
//!
//! ## Every fragment names its sender
//!
//! One fleet key covers up to 24 drones, so a broadcast request id can be
//! answered by more than one aircraft at once. Splicing two senders' symbols
//! into one decoder yields a body belonging to neither, and it decodes
//! *silently* — RaptorQ has no per-object checksum to catch it. The sender's
//! device id therefore leads every fragment, and the ground drops a fragment
//! whose sender is not the drone it addressed.

use raptorq::{Decoder, Encoder, EncodingPacket, ObjectTransmissionInformation, PayloadId};

use super::RpcCodecError;
use crate::aux_mux::AUX_MAX_PAYLOAD;
use crate::node_status::MAX_DEVICE_ID;

/// Fixed response-fragment overhead around the sender id and symbol bytes:
/// 1 sender_len, 4 id, 2 status, 2 frag_index, 2 frag_total, 4 oti, 2 frag_len.
pub const RPC_RESPONSE_OVERHEAD_BASE: usize = 17;

/// Repair symbols emitted alongside a response's systematic symbols.
///
/// Four is chosen against the measured 0.7% per-fragment loss and the 26-symbol
/// worst case: it is enough to make loss of the whole response negligible while
/// costing four frames on a body that already needs 26.
pub const RPC_REPAIR_SYMBOLS: u32 = 4;

/// Largest RaptorQ symbol one response fragment can carry.
///
/// The worst-case device id is budgeted whether or not the sender's id is that
/// long, so the symbol geometry a receiver derives never varies with the
/// sender's name. A receiver seeds its decoder from whichever fragment lands
/// first and must not have to wait for a particular one to learn the shape.
pub const MAX_RESPONSE_FRAGMENT: usize =
    AUX_MAX_PAYLOAD - RPC_RESPONSE_OVERHEAD_BASE - MAX_DEVICE_ID;

/// Most fragments one response may be split into.
///
/// Bounds a runaway reassembly buffer on the ground: a corrupt `frag_total` is
/// a number the radio handed us, not one we chose.
pub const MAX_RESPONSE_FRAGMENTS: usize = 64;

/// Largest body [`split_response`] will encode.
///
/// The repair symbols are fragments too, so they come out of the same ceiling:
/// 60 systematic symbols at [`MAX_RESPONSE_FRAGMENT`] each. The largest
/// endpoint measured on a live drone (`/api/status/full`, 29 339 B) needs 26,
/// so this is real headroom.
pub const MAX_RESPONSE_BODY: usize =
    (MAX_RESPONSE_FRAGMENTS - RPC_REPAIR_SYMBOLS as usize) * MAX_RESPONSE_FRAGMENT;

/// A decoded response fragment. Borrows the original payload buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcResponse<'a> {
    pub id: u32,
    /// Device id of the drone that answered.
    ///
    /// The ground drops a fragment whose sender is not the drone it addressed,
    /// so two aircraft answering one broadcast id cannot splice into one body.
    pub sender: &'a [u8],
    pub status: u16,
    /// Encoding-symbol id of this fragment, which is also its 0-based position
    /// in the transmitted sequence: `0..k` are systematic, the rest repair.
    pub index: u16,
    /// Fragments transmitted for this response, `k + RPC_REPAIR_SYMBOLS`.
    /// Any `k` of them rebuild the body, so this is a ceiling, not a quorum.
    pub total: u16,
    /// The body length, and with it the whole RaptorQ geometry. Identical on
    /// every fragment of one response, so a receiver seeds its decoder from
    /// whichever arrives first.
    pub oti: u32,
    /// This fragment's symbol, not the whole body.
    pub body: &'a [u8],
}

/// A response body encoded as the symbols that will carry it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseSymbols {
    /// The RaptorQ object-transmission information, as the 4 bytes every
    /// fragment carries.
    ///
    /// RFC 6330's OTI is 12 bytes, but eight of them are a build-time constant
    /// both ends share (one source block, one sub-block, byte alignment, and a
    /// symbol size derived from the body length), so the transfer length is the
    /// whole variable part and `u32` covers [`MAX_RESPONSE_BODY`] many times
    /// over.
    pub oti: u32,
    /// `k` systematic symbols followed by [`RPC_REPAIR_SYMBOLS`] repair
    /// symbols, in encoding-symbol-id order.
    pub symbols: Vec<Vec<u8>>,
}

/// The (systematic symbol count, symbol size) a body of `len` bytes uses.
///
/// Derived from the length alone, so the ground reconstructs it from the 4-byte
/// `oti` with no second wire field. Symbols are sized to spread the body evenly
/// across the fewest frames it needs rather than pinned at
/// [`MAX_RESPONSE_FRAGMENT`]: RaptorQ symbols are all one size, so a pinned
/// size would zero-pad a 200-byte answer into five full 1151-byte frames — 27x
/// the airtime, on the one lane a 24-drone fleet is airtime-bound by.
///
/// `len` must be non-zero; an empty body carries no symbols at all.
fn response_shape(len: usize) -> (usize, usize) {
    let k = len.div_ceil(MAX_RESPONSE_FRAGMENT);
    (k, len.div_ceil(k))
}

/// The RaptorQ config for a body of `len` bytes.
///
/// One source block, one sub-block, byte alignment: with a single block the
/// encoding-symbol id *is* the fragment index, so no per-packet RaptorQ payload
/// id has to ride the wire beside the index we already send.
fn response_config(len: u32, symbol_size: usize) -> ObjectTransmissionInformation {
    ObjectTransmissionInformation::new(len as u64, symbol_size as u16, 1, 1, 1)
}

/// Encode a response body as the symbols that will carry it.
///
/// An empty body yields exactly one empty symbol, so a 204 still travels as
/// `total = 1` and the ground completes the call instead of waiting for a
/// fragment that never comes. Returns `None` past [`MAX_RESPONSE_BODY`] — the
/// caller emits a 413 rather than a body the ground cannot bound.
pub fn split_response(body: &[u8]) -> Option<ResponseSymbols> {
    if body.is_empty() {
        // Nothing to protect and no block to decode. `oti = 0` is the empty
        // body, and the ground completes on the first fragment it sees.
        return Some(ResponseSymbols {
            oti: 0,
            symbols: vec![Vec::new()],
        });
    }
    if body.len() > MAX_RESPONSE_BODY {
        return None;
    }
    let (_, symbol_size) = response_shape(body.len());
    let encoder = Encoder::new(body, response_config(body.len() as u32, symbol_size));
    // Read the transfer length back off the encoder rather than assuming it:
    // the OTI on the wire is the one the symbols were actually built from.
    let oti = encoder.get_config().transfer_length() as u32;
    Some(ResponseSymbols {
        oti,
        symbols: encoder
            .get_encoded_packets(RPC_REPAIR_SYMBOLS)
            .into_iter()
            .map(|packet| packet.split().1)
            .collect(),
    })
}

/// Encode one response fragment as an aux payload.
///
/// Returns `None` when the sender id exceeds [`MAX_DEVICE_ID`], the fragment
/// does not fit one aux datagram, `total` is zero, or `index` is not inside
/// `total`.
pub fn encode_response_fragment(
    sender: &[u8],
    id: u32,
    status: u16,
    index: u16,
    total: u16,
    oti: u32,
    symbol: &[u8],
) -> Option<Vec<u8>> {
    if sender.len() > MAX_DEVICE_ID {
        return None;
    }
    if total == 0 || index >= total {
        return None;
    }
    if symbol.len() > u16::MAX as usize {
        return None;
    }
    let encoded_len = RPC_RESPONSE_OVERHEAD_BASE + sender.len() + symbol.len();
    if encoded_len > AUX_MAX_PAYLOAD {
        return None;
    }
    let mut out = Vec::with_capacity(encoded_len);
    out.push(sender.len() as u8);
    out.extend_from_slice(sender);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&status.to_be_bytes());
    out.extend_from_slice(&index.to_be_bytes());
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&oti.to_be_bytes());
    out.extend_from_slice(&(symbol.len() as u16).to_be_bytes());
    out.extend_from_slice(symbol);
    Some(out)
}

/// Decode an aux payload as a response fragment.
pub fn decode_response(payload: &[u8]) -> Result<RpcResponse<'_>, RpcCodecError> {
    let &sender_len = payload.first().ok_or(RpcCodecError::TooShort)?;
    if sender_len as usize > MAX_DEVICE_ID {
        return Err(RpcCodecError::BadSenderLen(sender_len));
    }
    let sender_len = sender_len as usize;
    let header = RPC_RESPONSE_OVERHEAD_BASE + sender_len;
    if payload.len() < header {
        return Err(RpcCodecError::TooShort);
    }
    let sender = &payload[1..1 + sender_len];
    // The 16 fixed bytes between the sender id and the symbol.
    let fixed = &payload[1 + sender_len..];
    let id = u32::from_be_bytes([fixed[0], fixed[1], fixed[2], fixed[3]]);
    let status = u16::from_be_bytes([fixed[4], fixed[5]]);
    let index = u16::from_be_bytes([fixed[6], fixed[7]]);
    let total = u16::from_be_bytes([fixed[8], fixed[9]]);
    let oti = u32::from_be_bytes([fixed[10], fixed[11], fixed[12], fixed[13]]);
    let frag_len = u16::from_be_bytes([fixed[14], fixed[15]]) as usize;
    if header + frag_len != payload.len() {
        return Err(RpcCodecError::LengthMismatch {
            declared: header + frag_len,
            actual: payload.len(),
        });
    }
    if total == 0 || index >= total {
        return Err(RpcCodecError::BadFragmentIndex { index, total });
    }
    Ok(RpcResponse {
        id,
        sender,
        status,
        index,
        total,
        oti,
        body: &payload[header..],
    })
}

/// What one fragment did to a [`ResponseDecoder`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FragmentOutcome {
    /// Accepted; more symbols are needed before the body rebuilds.
    Pending,
    /// Accepted, and it was the symbol that closed the block.
    Complete(Vec<u8>),
    /// Not a symbol this response could contain. Radio damage or a peer on a
    /// different build. Dropped rather than fed to RaptorQ, which assumes every
    /// symbol is one size and asserts on an out-of-range encoding-symbol id.
    BadSymbol,
}

/// Ground-side reassembler for one response.
///
/// Seeded from any single fragment's `oti`, then fed fragments in whatever
/// order the lane delivers them. Completes the moment enough symbols have
/// arrived, which is `k` of the `k + RPC_REPAIR_SYMBOLS` transmitted — waiting
/// for every index is exactly the geometric decay the repair symbols exist to
/// remove.
#[derive(Debug)]
pub struct ResponseDecoder {
    /// `None` for an empty body, which has no block to decode.
    inner: Option<Decoder>,
    symbol_size: usize,
    /// One past the largest encoding-symbol id this response can contain.
    symbol_id_limit: u32,
}

impl ResponseDecoder {
    /// A decoder for a response whose fragments carry `oti`.
    ///
    /// Returns `None` for a transfer length past [`MAX_RESPONSE_BODY`]. The
    /// field arrives off the radio and RaptorQ sizes its matrices from it, so
    /// an unbounded value is an allocation the ground refuses rather than
    /// attempts.
    pub fn new(oti: u32) -> Option<Self> {
        let len = oti as usize;
        if len > MAX_RESPONSE_BODY {
            return None;
        }
        if len == 0 {
            return Some(Self {
                inner: None,
                symbol_size: 0,
                symbol_id_limit: RPC_REPAIR_SYMBOLS,
            });
        }
        let (k, symbol_size) = response_shape(len);
        Some(Self {
            inner: Some(Decoder::new(response_config(oti, symbol_size))),
            symbol_size,
            symbol_id_limit: k as u32 + RPC_REPAIR_SYMBOLS,
        })
    }

    /// The symbol size every fragment of this response carries.
    pub fn symbol_size(&self) -> usize {
        self.symbol_size
    }

    /// Feed one fragment.
    pub fn push(&mut self, index: u16, symbol: &[u8]) -> FragmentOutcome {
        if symbol.len() != self.symbol_size || index as u32 >= self.symbol_id_limit {
            return FragmentOutcome::BadSymbol;
        }
        let Some(decoder) = self.inner.as_mut() else {
            return FragmentOutcome::Complete(Vec::new());
        };
        let packet = EncodingPacket::new(PayloadId::new(0, index as u32), symbol.to_vec());
        match decoder.decode(packet) {
            Some(body) => FragmentOutcome::Complete(body),
            None => FragmentOutcome::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SENDER: &[u8] = b"ados-abc123";

    /// Encode a whole body the way the drone does, then decode it the way the
    /// ground does, delivering `order` and skipping `drop`.
    fn round_trip(body: &[u8], order: &[usize], drop: &[usize]) -> Option<Vec<u8>> {
        let split = split_response(body).unwrap();
        let total = split.symbols.len() as u16;
        let mut decoder: Option<ResponseDecoder> = None;
        for &i in order {
            if drop.contains(&i) {
                continue;
            }
            let payload = encode_response_fragment(
                SENDER,
                7,
                200,
                i as u16,
                total,
                split.oti,
                &split.symbols[i],
            )
            .unwrap();
            let frag = decode_response(&payload).unwrap();
            assert_eq!(frag.sender, SENDER);
            assert_eq!(frag.oti, split.oti);
            assert_eq!(frag.total, total);
            let d = decoder.get_or_insert_with(|| ResponseDecoder::new(frag.oti).unwrap());
            if let FragmentOutcome::Complete(out) = d.push(frag.index, frag.body) {
                return Some(out);
            }
        }
        None
    }

    #[test]
    fn round_trips_a_single_fragment_response() {
        let body = br#"{"device_id":"abc","name":"Skynode A7S"}"#;
        let split = split_response(body).unwrap();
        assert_eq!(split.oti, body.len() as u32);
        let enc = encode_response_fragment(
            SENDER,
            42,
            200,
            0,
            split.symbols.len() as u16,
            split.oti,
            &split.symbols[0],
        )
        .unwrap();
        let dec = decode_response(&enc).unwrap();
        assert_eq!(dec.id, 42);
        assert_eq!(dec.sender, SENDER);
        assert_eq!(dec.status, 200);
        assert_eq!(dec.index, 0);
        assert_eq!(round_trip(body, &[0], &[]).as_deref(), Some(&body[..]));
    }

    #[test]
    fn an_empty_body_still_produces_one_fragment() {
        // A 204 must complete the caller, not leave it waiting for a fragment
        // that never comes.
        let split = split_response(&[]).unwrap();
        assert_eq!(split.oti, 0);
        assert_eq!(split.symbols.len(), 1);
        assert!(split.symbols[0].is_empty());
        let enc = encode_response_fragment(SENDER, 7, 204, 0, 1, 0, &split.symbols[0]).unwrap();
        let dec = decode_response(&enc).unwrap();
        assert_eq!(dec.status, 204);
        assert_eq!(dec.total, 1);
        assert!(dec.body.is_empty());
        let mut decoder = ResponseDecoder::new(dec.oti).unwrap();
        assert_eq!(
            decoder.push(dec.index, dec.body),
            FragmentOutcome::Complete(Vec::new())
        );
    }

    #[test]
    fn a_response_carries_k_systematic_plus_the_repair_symbols() {
        let body: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        let split = split_response(&body).unwrap();
        let k = body.len().div_ceil(MAX_RESPONSE_FRAGMENT);
        assert_eq!(k, 3);
        assert_eq!(split.symbols.len(), k + RPC_REPAIR_SYMBOLS as usize);
        // Every symbol is one size — RaptorQ requires it, and the ground
        // rejects any fragment that is not.
        let size = split.symbols[0].len();
        assert!(split.symbols.iter().all(|s| s.len() == size));
        assert!(size <= MAX_RESPONSE_FRAGMENT);
    }

    #[test]
    fn a_thirty_kilobyte_body_survives_losing_any_four_fragments() {
        // The measured `/api/status/full`. All-or-nothing reassembly of its 26
        // fragments at 0.7% per-fragment loss is the 85% success measured on the
        // bench; the repair symbols are what remove that decay.
        let body: Vec<u8> = (0..29_339u32).map(|i| (i % 251) as u8).collect();
        let split = split_response(&body).unwrap();
        assert_eq!(split.symbols.len(), 30, "26 systematic + 4 repair");
        let order: Vec<usize> = (0..30).collect();
        for drop in [
            vec![0usize, 1, 2, 3],
            vec![26usize, 27, 28, 29],
            vec![0usize, 9, 17, 29],
            vec![5usize, 6, 7, 25],
        ] {
            assert_eq!(
                round_trip(&body, &order, &drop).as_deref(),
                Some(&body[..]),
                "dropping {drop:?}"
            );
        }
    }

    #[test]
    fn fragments_rejoin_out_of_order() {
        let body: Vec<u8> = (0..8000u32).map(|i| (i % 251) as u8).collect();
        let split = split_response(&body).unwrap();
        let mut order: Vec<usize> = (0..split.symbols.len()).collect();
        order.reverse();
        assert_eq!(round_trip(&body, &order, &[]).as_deref(), Some(&body[..]));
    }

    #[test]
    fn a_body_past_the_reassembly_ceiling_does_not_split() {
        assert!(split_response(&vec![0u8; MAX_RESPONSE_BODY + 1]).is_none());
        let at_ceiling = vec![0u8; MAX_RESPONSE_BODY];
        assert_eq!(
            split_response(&at_ceiling).unwrap().symbols.len(),
            MAX_RESPONSE_FRAGMENTS
        );
    }

    #[test]
    fn rejects_a_fragment_larger_than_one_aux_frame() {
        let big = vec![0u8; MAX_RESPONSE_FRAGMENT + 1];
        let sender = vec![b'a'; MAX_DEVICE_ID];
        assert!(encode_response_fragment(&sender, 1, 200, 0, 1, 1, &big).is_none());
        let fits = vec![0u8; MAX_RESPONSE_FRAGMENT];
        let enc = encode_response_fragment(&sender, 1, 200, 0, 1, 1, &fits).unwrap();
        assert_eq!(enc.len(), AUX_MAX_PAYLOAD, "the budget is exact");
        assert_eq!(
            decode_response(&enc).unwrap().body.len(),
            MAX_RESPONSE_FRAGMENT
        );
    }

    #[test]
    fn rejects_a_sender_longer_than_a_device_id() {
        let too_long = vec![b'a'; MAX_DEVICE_ID + 1];
        assert!(encode_response_fragment(&too_long, 1, 200, 0, 1, 1, b"x").is_none());
        // A hand-built frame claiming an oversized sender must not decode: the
        // length prefix indexes the buffer, so an unchecked one reads a symbol
        // out of the sender's bytes.
        let mut bad = vec![(MAX_DEVICE_ID + 1) as u8];
        bad.extend_from_slice(&too_long);
        bad.extend_from_slice(&[0u8; 16]);
        assert_eq!(
            decode_response(&bad).unwrap_err(),
            RpcCodecError::BadSenderLen((MAX_DEVICE_ID + 1) as u8)
        );
    }

    #[test]
    fn rejects_a_fragment_whose_index_is_outside_its_total() {
        assert!(encode_response_fragment(SENDER, 1, 200, 3, 3, 1, b"x").is_none());
        assert!(encode_response_fragment(SENDER, 1, 200, 0, 0, 1, b"x").is_none());
        // Hand-built frame claiming index 4 of 2 must not decode.
        let mut bad = vec![0u8];
        bad.extend_from_slice(&1u32.to_be_bytes());
        bad.extend_from_slice(&200u16.to_be_bytes());
        bad.extend_from_slice(&4u16.to_be_bytes());
        bad.extend_from_slice(&2u16.to_be_bytes());
        bad.extend_from_slice(&1u32.to_be_bytes());
        bad.extend_from_slice(&1u16.to_be_bytes());
        bad.push(b'x');
        assert_eq!(
            decode_response(&bad).unwrap_err(),
            RpcCodecError::BadFragmentIndex { index: 4, total: 2 }
        );
    }

    #[test]
    fn rejects_a_truncated_response() {
        let full = encode_response_fragment(SENDER, 9, 200, 0, 1, 4, b"body").unwrap();
        for cut in [0usize, 1, 4, RPC_RESPONSE_OVERHEAD_BASE + SENDER.len() - 1] {
            let mut truncated = full.clone();
            truncated.truncate(cut);
            assert_eq!(
                decode_response(&truncated).unwrap_err(),
                RpcCodecError::TooShort,
                "cut to {cut}"
            );
        }
    }

    #[test]
    fn catches_a_length_mismatch_in_a_response() {
        // Claimed frag_len = 50 but the payload ends after the header.
        let mut bad = vec![0u8];
        bad.extend_from_slice(&1u32.to_be_bytes());
        bad.extend_from_slice(&200u16.to_be_bytes());
        bad.extend_from_slice(&0u16.to_be_bytes());
        bad.extend_from_slice(&1u16.to_be_bytes());
        bad.extend_from_slice(&50u32.to_be_bytes());
        bad.extend_from_slice(&50u16.to_be_bytes());
        assert_eq!(
            decode_response(&bad).unwrap_err(),
            RpcCodecError::LengthMismatch {
                declared: RPC_RESPONSE_OVERHEAD_BASE + 50,
                actual: RPC_RESPONSE_OVERHEAD_BASE
            }
        );
    }

    #[test]
    fn an_impossible_index_total_pair_is_its_own_fault_not_a_length_fault() {
        // index 3 of total 2. Reported as a length mismatch, the numbers read
        // as byte counts and the real fault is invisible in the log line.
        let mut bad = vec![0u8];
        bad.extend_from_slice(&1u32.to_be_bytes());
        bad.extend_from_slice(&200u16.to_be_bytes());
        bad.extend_from_slice(&3u16.to_be_bytes());
        bad.extend_from_slice(&2u16.to_be_bytes());
        bad.extend_from_slice(&0u32.to_be_bytes());
        bad.extend_from_slice(&0u16.to_be_bytes());
        assert_eq!(
            decode_response(&bad).unwrap_err(),
            RpcCodecError::BadFragmentIndex { index: 3, total: 2 }
        );
    }

    #[test]
    fn a_decoder_refuses_an_unbounded_transfer_length() {
        // The field arrives off the radio. RaptorQ sizes its matrices from it,
        // so an unchecked value is an allocation a corrupt frame can demand.
        assert!(ResponseDecoder::new(u32::MAX).is_none());
        assert!(ResponseDecoder::new(MAX_RESPONSE_BODY as u32 + 1).is_none());
        assert!(ResponseDecoder::new(MAX_RESPONSE_BODY as u32).is_some());
    }

    #[test]
    fn a_decoder_drops_a_symbol_of_the_wrong_size_or_id() {
        let body: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let split = split_response(&body).unwrap();
        let mut decoder = ResponseDecoder::new(split.oti).unwrap();
        let size = decoder.symbol_size();
        assert_eq!(size, split.symbols[0].len());
        // A short symbol would be fed into a matrix that assumes one size.
        assert_eq!(
            decoder.push(0, &vec![0u8; size - 1]),
            FragmentOutcome::BadSymbol
        );
        // An encoding-symbol id past what this response can contain: `total`
        // comes off the radio, and RaptorQ asserts on a wild id.
        let k = body.len().div_ceil(MAX_RESPONSE_FRAGMENT);
        assert_eq!(
            decoder.push((k + RPC_REPAIR_SYMBOLS as usize) as u16, &vec![0u8; size]),
            FragmentOutcome::BadSymbol
        );
        // Rejecting them left the decoder usable.
        for (i, symbol) in split.symbols.iter().enumerate() {
            if let FragmentOutcome::Complete(out) = decoder.push(i as u16, symbol) {
                assert_eq!(out, body);
                return;
            }
        }
        panic!("the full symbol set must decode");
    }

    #[test]
    fn every_body_size_across_the_fragment_range_round_trips() {
        // The k boundaries are where the derived symbol size changes, and a
        // shape the two ends disagree on decodes to silent garbage.
        for k in [1usize, 2, 26, 60] {
            for len in [
                (k - 1) * MAX_RESPONSE_FRAGMENT + 1,
                k * MAX_RESPONSE_FRAGMENT,
            ] {
                let body: Vec<u8> = (0..len).map(|i| ((i * 7 + 13) % 251) as u8).collect();
                let split = split_response(&body).unwrap();
                assert_eq!(
                    split.symbols.len(),
                    k + RPC_REPAIR_SYMBOLS as usize,
                    "len {len}"
                );
                let order: Vec<usize> = (0..split.symbols.len()).collect();
                assert_eq!(
                    round_trip(&body, &order, &[0, 1]).as_deref(),
                    Some(&body[..]),
                    "len {len}"
                );
            }
        }
    }

    #[test]
    fn frame_budgets_account_for_fixed_overhead() {
        // A response fragment's fixed overhead is 17 bytes: 1 sender_len,
        // 4 id, 2 status, 2 frag_index, 2 frag_total, 4 oti, 2 frag_len. The
        // worst-case 32-byte device id is budgeted on every fragment whether or
        // not this sender's id is that long, so the symbol size both ends derive
        // from `oti` never varies with the sender's name.
        assert_eq!(RPC_RESPONSE_OVERHEAD_BASE, 17);
        assert_eq!(MAX_RESPONSE_FRAGMENT, 1200 - 17 - 32);
        assert_eq!(MAX_RESPONSE_FRAGMENT, 1151);
        // The repair symbols come out of the fragment ceiling too.
        assert_eq!(MAX_RESPONSE_BODY, 60 * 1151);
        // The ceiling must cover the largest endpoint measured on a live drone
        // (/api/status/full at 29 339 B), which needs 26 systematic symbols.
        const { assert!(MAX_RESPONSE_BODY > 29_339) };
        assert_eq!(29_339usize.div_ceil(MAX_RESPONSE_FRAGMENT), 26);
    }
}
