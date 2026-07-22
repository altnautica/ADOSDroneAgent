//! CRSF frame layer: the byte framing, the CRC, and a sync-hunting stream
//! parser that never panics on garbage.
//!
//! Every CRSF frame is `[sync] [len] [type] [payload…] [crc8]` where `len`
//! counts the bytes AFTER it (type + payload + crc, so 2..=62) and the CRC-8
//! (DVB-S2 polynomial `0xD5`, init 0, no reflection, no xor-out) covers the
//! type and payload bytes only — the sync and length bytes are excluded.
//!
//! Sync-byte decision, recorded: the leading byte is a CRSF device address,
//! not a fixed magic. Three addresses appear on a real shared bus — `0xC8`
//! (flight controller), `0xEE` (transmitter module), and `0xEA` (radio
//! handset) — so the parser accepts any of the three and rejects everything
//! else. Frames this service transmits toward the RC module carry `0xC8`.

/// CRSF device address: flight controller. Also the sync byte on frames the
/// handset/controller side sends to the TX module.
pub const ADDR_FLIGHT_CONTROLLER: u8 = 0xC8;
/// CRSF device address: RC transmitter module.
pub const ADDR_TRANSMITTER_MODULE: u8 = 0xEE;
/// CRSF device address: radio handset.
pub const ADDR_HANDSET: u8 = 0xEA;

/// Frame type: RC channels, 16 × 11-bit packed.
pub const TYPE_RC_CHANNELS_PACKED: u8 = 0x16;
/// Frame type: GPS telemetry.
pub const TYPE_GPS: u8 = 0x02;
/// Frame type: battery telemetry.
pub const TYPE_BATTERY: u8 = 0x08;
/// Frame type: link statistics telemetry.
pub const TYPE_LINK_STATISTICS: u8 = 0x14;
/// Frame type: attitude telemetry.
pub const TYPE_ATTITUDE: u8 = 0x1E;
/// Frame type: flight-mode telemetry (null-terminated ASCII).
pub const TYPE_FLIGHT_MODE: u8 = 0x21;
/// Frame type: parameter settings entry (extended frame).
pub const TYPE_PARAMETER_SETTINGS_ENTRY: u8 = 0x2D;
/// Frame type: parameter read request (extended frame).
pub const TYPE_PARAMETER_READ: u8 = 0x2E;
/// Frame type: parameter write command (extended frame).
pub const TYPE_PARAMETER_WRITE: u8 = 0x2F;

/// Maximum value of the length byte (type + payload + crc).
pub const MAX_LEN_BYTE: u8 = 62;
/// Minimum value of the length byte (type + crc, empty payload).
pub const MIN_LEN_BYTE: u8 = 2;
/// Maximum payload size (`MAX_LEN_BYTE` minus the type and crc bytes).
pub const MAX_PAYLOAD: usize = 60;
/// Maximum whole-frame size on the wire (sync + len + 62).
pub const MAX_FRAME_BYTES: usize = 64;

/// CRC-8/DVB-S2 over `data`: polynomial `0xD5`, init `0x00`, not reflected,
/// no final xor. Computed over the type byte plus the payload bytes of a frame.
pub fn crc8_dvb_s2(data: &[u8]) -> u8 {
    let mut crc: u8 = 0;
    for &byte in data {
        crc ^= byte;
        for _ in 0..8 {
            if crc & 0x80 != 0 {
                crc = (crc << 1) ^ 0xD5;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

/// A validated frame lifted off the wire: the address (sync) byte, the frame
/// type, and the raw payload. Typed decoding of the payload lives in the
/// sibling `telemetry` / `channels` / `params` modules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFrame {
    pub sync: u8,
    pub frame_type: u8,
    pub payload: Vec<u8>,
}

/// Why a byte sequence was rejected as a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// The leading byte is not a known CRSF device address.
    BadSync(u8),
    /// The length byte is outside 2..=62.
    BadLength(u8),
    /// Fewer bytes than the length byte promises.
    Truncated,
    /// The trailing CRC does not match the computed one.
    CrcMismatch { expected: u8, got: u8 },
    /// A known frame type carried a payload of the wrong size.
    PayloadSizeMismatch {
        frame_type: u8,
        expected: usize,
        got: usize,
    },
    /// A payload longer than the wire format can carry was offered for build.
    PayloadTooLarge(usize),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::BadSync(b) => write!(f, "bad sync byte 0x{b:02X}"),
            FrameError::BadLength(l) => write!(f, "bad length byte {l}"),
            FrameError::Truncated => write!(f, "truncated frame"),
            FrameError::CrcMismatch { expected, got } => {
                write!(
                    f,
                    "crc mismatch: expected 0x{expected:02X}, got 0x{got:02X}"
                )
            }
            FrameError::PayloadSizeMismatch {
                frame_type,
                expected,
                got,
            } => write!(
                f,
                "payload size mismatch for type 0x{frame_type:02X}: expected {expected}, got {got}"
            ),
            FrameError::PayloadTooLarge(n) => write!(f, "payload too large: {n} bytes"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Whether `b` is one of the three device addresses accepted as a sync byte.
pub fn is_sync_byte(b: u8) -> bool {
    matches!(
        b,
        ADDR_FLIGHT_CONTROLLER | ADDR_TRANSMITTER_MODULE | ADDR_HANDSET
    )
}

/// The fixed payload size for a known frame type, `None` for variable-size or
/// unknown types (which pass through with any in-range payload).
pub fn fixed_payload_size(frame_type: u8) -> Option<usize> {
    match frame_type {
        TYPE_RC_CHANNELS_PACKED => Some(22),
        TYPE_LINK_STATISTICS => Some(10),
        TYPE_GPS => Some(15),
        TYPE_BATTERY => Some(8),
        TYPE_ATTITUDE => Some(6),
        _ => None,
    }
}

/// Build a complete wire frame: `[sync, len, type, payload…, crc]`.
pub fn build_frame(sync: u8, frame_type: u8, payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    if payload.len() > MAX_PAYLOAD {
        return Err(FrameError::PayloadTooLarge(payload.len()));
    }
    let mut out = Vec::with_capacity(4 + payload.len());
    out.push(sync);
    out.push((payload.len() + 2) as u8); // type + payload + crc
    out.push(frame_type);
    out.extend_from_slice(payload);
    out.push(crc8_dvb_s2(&out[2..]));
    Ok(out)
}

/// Parse and validate one complete frame from the FRONT of `buf`.
///
/// On success returns the frame plus the number of bytes consumed. Applies the
/// four validation rules: sync byte is a known device address; length byte in
/// 2..=62; CRC over type+payload matches; a known fixed-size type carries
/// exactly its expected payload.
pub fn parse_frame(buf: &[u8]) -> Result<(RawFrame, usize), FrameError> {
    if buf.len() < 2 {
        return Err(FrameError::Truncated);
    }
    let sync = buf[0];
    if !is_sync_byte(sync) {
        return Err(FrameError::BadSync(sync));
    }
    let len = buf[1];
    if !(MIN_LEN_BYTE..=MAX_LEN_BYTE).contains(&len) {
        return Err(FrameError::BadLength(len));
    }
    let total = 2 + len as usize;
    if buf.len() < total {
        return Err(FrameError::Truncated);
    }
    let frame_type = buf[2];
    let payload = &buf[3..total - 1];
    let got_crc = buf[total - 1];
    let expected_crc = crc8_dvb_s2(&buf[2..total - 1]);
    if got_crc != expected_crc {
        return Err(FrameError::CrcMismatch {
            expected: expected_crc,
            got: got_crc,
        });
    }
    if let Some(expected) = fixed_payload_size(frame_type) {
        if payload.len() != expected {
            return Err(FrameError::PayloadSizeMismatch {
                frame_type,
                expected,
                got: payload.len(),
            });
        }
    }
    Ok((
        RawFrame {
            sync,
            frame_type,
            payload: payload.to_vec(),
        },
        total,
    ))
}

/// Incremental sync-hunting parser for a CRSF byte stream.
///
/// Feed arbitrary chunks with [`Parser::push`]; completed valid frames come
/// back in order. Invalid bytes are skipped one at a time (resync from the
/// next byte) and CRC failures are counted, so garbage input can never panic
/// or wedge the stream. The internal buffer is inherently bounded: the push
/// loop drains every parseable or rejectable prefix and only stops on a
/// truncated frame candidate, which by the length rule needs fewer than
/// [`MAX_FRAME_BYTES`] bytes — so the buffer never holds a full frame's worth
/// of bytes across calls.
#[derive(Debug, Default)]
pub struct Parser {
    buf: Vec<u8>,
    /// Valid frames produced since construction.
    pub frames_ok: u64,
    /// Frames rejected for a CRC mismatch.
    pub crc_errors: u64,
    /// Bytes skipped hunting for a sync byte plus frames rejected for a bad
    /// length or a known-type payload-size mismatch.
    pub resync_bytes: u64,
}

impl Parser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume a chunk of the stream, returning every completed valid frame.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<RawFrame> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            match parse_frame(&self.buf) {
                Ok((frame, consumed)) => {
                    self.buf.drain(..consumed);
                    self.frames_ok += 1;
                    out.push(frame);
                }
                Err(FrameError::Truncated) => break,
                Err(e) => {
                    if matches!(e, FrameError::CrcMismatch { .. }) {
                        self.crc_errors += 1;
                    } else {
                        self.resync_bytes += 1;
                    }
                    // Drop one byte and hunt for the next sync candidate. On a
                    // CRC failure this re-scans the corrupt frame's interior,
                    // which is the safe choice: a corrupted length byte may
                    // have swallowed a real frame boundary.
                    self.buf.drain(..1);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CRC-8/DVB-S2 catalog check value: the CRC of the ASCII digits
    /// "123456789" is 0xBC for this polynomial/init/reflection combination.
    /// An oracle independent of this implementation.
    #[test]
    fn crc8_catalog_check_value() {
        assert_eq!(crc8_dvb_s2(b"123456789"), 0xBC);
    }

    /// Hand-derived single-byte values from the 0xD5 polynomial: 0x00 stays 0;
    /// 0x01 shifts left seven times to 0x80, then the eighth shift reduces by
    /// the polynomial to 0xD5.
    #[test]
    fn crc8_single_byte_vectors() {
        assert_eq!(crc8_dvb_s2(&[0x00]), 0x00);
        assert_eq!(crc8_dvb_s2(&[0x01]), 0xD5);
    }

    #[test]
    fn crc8_empty_input_is_zero() {
        assert_eq!(crc8_dvb_s2(&[]), 0x00);
    }

    #[test]
    fn build_frame_layout_and_roundtrip() {
        let payload = [0xAA, 0xBB, 0xCC];
        let wire = build_frame(ADDR_FLIGHT_CONTROLLER, 0x7F, &payload).unwrap();
        // sync + len + type + payload + crc
        assert_eq!(wire.len(), 2 + 2 + payload.len());
        assert_eq!(wire[0], 0xC8);
        assert_eq!(wire[1], (payload.len() + 2) as u8);
        assert_eq!(wire[2], 0x7F);
        let (frame, consumed) = parse_frame(&wire).unwrap();
        assert_eq!(consumed, wire.len());
        assert_eq!(frame.sync, 0xC8);
        assert_eq!(frame.frame_type, 0x7F);
        assert_eq!(frame.payload, payload);
    }

    #[test]
    fn build_frame_rejects_oversize_payload() {
        let payload = vec![0u8; MAX_PAYLOAD + 1];
        assert_eq!(
            build_frame(ADDR_FLIGHT_CONTROLLER, 0x10, &payload),
            Err(FrameError::PayloadTooLarge(MAX_PAYLOAD + 1))
        );
        // The maximum payload still builds and parses.
        let max = vec![0u8; MAX_PAYLOAD];
        let wire = build_frame(ADDR_FLIGHT_CONTROLLER, 0x10, &max).unwrap();
        assert_eq!(wire[1], MAX_LEN_BYTE);
        assert!(parse_frame(&wire).is_ok());
    }

    #[test]
    fn parse_rejects_corrupted_crc() {
        let mut wire = build_frame(ADDR_FLIGHT_CONTROLLER, 0x7F, &[1, 2, 3]).unwrap();
        let last = wire.len() - 1;
        wire[last] ^= 0xFF;
        assert!(matches!(
            parse_frame(&wire),
            Err(FrameError::CrcMismatch { .. })
        ));
    }

    #[test]
    fn parse_rejects_bad_sync_and_bad_length() {
        assert_eq!(parse_frame(&[0x55, 0x04]), Err(FrameError::BadSync(0x55)));
        assert_eq!(
            parse_frame(&[ADDR_FLIGHT_CONTROLLER, 0x01, 0x00]),
            Err(FrameError::BadLength(0x01))
        );
        assert_eq!(
            parse_frame(&[ADDR_FLIGHT_CONTROLLER, 63, 0x00]),
            Err(FrameError::BadLength(63))
        );
    }

    #[test]
    fn parse_accepts_all_three_device_addresses() {
        for sync in [
            ADDR_FLIGHT_CONTROLLER,
            ADDR_TRANSMITTER_MODULE,
            ADDR_HANDSET,
        ] {
            let wire = build_frame(sync, 0x7F, &[9]).unwrap();
            assert_eq!(parse_frame(&wire).unwrap().0.sync, sync);
        }
    }

    #[test]
    fn parse_rejects_known_type_size_mismatch() {
        // A link-statistics frame must carry exactly 10 payload bytes.
        let wire = build_frame(ADDR_FLIGHT_CONTROLLER, TYPE_LINK_STATISTICS, &[0; 9]).unwrap();
        assert_eq!(
            parse_frame(&wire),
            Err(FrameError::PayloadSizeMismatch {
                frame_type: TYPE_LINK_STATISTICS,
                expected: 10,
                got: 9,
            })
        );
    }

    #[test]
    fn parser_reassembles_split_frames() {
        let wire = build_frame(ADDR_FLIGHT_CONTROLLER, 0x7F, &[1, 2, 3, 4]).unwrap();
        let mut p = Parser::new();
        // Feed one byte at a time; the frame completes only on the last byte.
        for &b in &wire[..wire.len() - 1] {
            assert!(p.push(&[b]).is_empty());
        }
        let frames = p.push(&[wire[wire.len() - 1]]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].payload, vec![1, 2, 3, 4]);
        assert_eq!(p.frames_ok, 1);
        assert_eq!(p.crc_errors, 0);
    }

    #[test]
    fn parser_recovers_after_garbage_and_counts_crc_errors() {
        let good = build_frame(ADDR_FLIGHT_CONTROLLER, 0x7F, &[7, 8]).unwrap();
        let mut corrupt = good.clone();
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0x01;
        let mut stream = Vec::new();
        stream.extend_from_slice(&[0x00, 0x11, 0x22]); // leading garbage
        stream.extend_from_slice(&corrupt);
        stream.extend_from_slice(&[0xFF]); // interstitial garbage
        stream.extend_from_slice(&good);
        let mut p = Parser::new();
        let frames = p.push(&stream);
        assert_eq!(frames.len(), 1, "only the intact frame survives");
        assert_eq!(frames[0].payload, vec![7, 8]);
        assert!(p.crc_errors >= 1, "the corrupted frame is counted");
        assert!(p.resync_bytes >= 3, "garbage bytes are counted");
    }

    #[test]
    fn parser_buffer_stays_bounded_under_garbage_floods() {
        let mut p = Parser::new();
        // Random-ish non-frame bytes in large chunks: every push must drain
        // to below one frame's worth of pending bytes (the truncated-front
        // invariant), so memory cannot grow with the flood.
        for i in 0..100u32 {
            let chunk: Vec<u8> = (0..512u32)
                .map(|j| ((i * 31 + j * 7) % 251) as u8)
                .collect();
            p.push(&chunk);
            assert!(p.buf.len() < MAX_FRAME_BYTES);
        }
    }
}
