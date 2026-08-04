//! MSP (MultiWii Serial Protocol) v1 + v2 codec.
//!
//! The agent's MAVLink router already exposes a raw MSP byte pipe on
//! `/run/ados/msp.sock` (it forwards bytes to and from a Betaflight / iNav /
//! KISS flight controller's serial link), but nothing in the tree could build
//! or parse an MSP frame. This module is that codec: the wire framing for both
//! MSP versions plus the one command a companion needs to fly a rate-control FC,
//! `MSP_SET_RAW_RC` (200), with the stick scaling the GCS already standardized.
//!
//! Two frame formats, both little-endian:
//!
//! * **MSPv1** `$M<`: `'$' 'M' <dir> <size:u8> <cmd:u8> <payload...> <crc:u8>`
//!   where `crc` is the XOR of `size`, `cmd`, and every payload byte. `size` is
//!   the payload length, so a v1 command id and a single payload byte cannot
//!   exceed 255. `<dir>` is `<` for a request to the FC, `>` for a reply, `!`
//!   for an error reply.
//! * **MSPv2** `$X<`: `'$' 'X' <dir> <flag:u8> <cmd:u16 LE> <size:u16 LE>
//!   <payload...> <crc:u8>` where `crc` is CRC8/DVB-S2 (poly `0xD5`) accumulated
//!   over `flag`, the two command bytes, the two size bytes, and every payload
//!   byte. v2 lifts the 8-bit command/size ceiling (needed for the full command
//!   space and larger settings payloads).
//!
//! The codec is transport-agnostic: it produces and consumes byte buffers, and
//! the plugin host / MSP client move those over the socket. It never opens the
//! socket or the FC itself.

/// `MSP_SET_RAW_RC` — write raw RC channel values (PWM microseconds) to the FC.
/// Payload is N little-endian `u16` channels. This is the command a companion
/// uses to fly an FC in a rate mode (ACRO), the FC treating the values as if
/// they came from a receiver.
pub const MSP_SET_RAW_RC: u16 = 200;

/// The v1 direction/marker byte for a request to the FC (`<`).
pub const DIR_TO_FC: u8 = b'<';
/// The v1 marker for a normal reply from the FC (`>`).
pub const DIR_FROM_FC: u8 = b'>';
/// The v1 marker for an error reply from the FC (`!`).
pub const DIR_ERROR: u8 = b'!';

/// Accumulate one byte into a CRC8/DVB-S2 running value (polynomial `0xD5`).
/// Seed with `0` and fold every byte of the v2 header+payload.
pub fn crc8_dvb_s2(mut crc: u8, byte: u8) -> u8 {
    crc ^= byte;
    for _ in 0..8 {
        if crc & 0x80 != 0 {
            crc = (crc << 1) ^ 0xD5;
        } else {
            crc <<= 1;
        }
    }
    crc
}

/// CRC8/DVB-S2 over a slice, seeded at 0.
pub fn crc8_dvb_s2_buf(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |c, &b| crc8_dvb_s2(c, b))
}

/// Encode an MSPv1 request `$M<`. `cmd` and `payload.len()` must each fit in a
/// byte; a command or payload that does not is a v2-only frame, so this returns
/// `None` rather than truncating (a silent truncation would corrupt the FC
/// command — fail loudly instead).
pub fn encode_v1(cmd: u16, payload: &[u8]) -> Option<Vec<u8>> {
    if cmd > u8::MAX as u16 || payload.len() > u8::MAX as usize {
        return None;
    }
    let size = payload.len() as u8;
    let cmd = cmd as u8;
    let mut out = Vec::with_capacity(6 + payload.len());
    out.extend_from_slice(&[b'$', b'M', DIR_TO_FC, size, cmd]);
    out.extend_from_slice(payload);
    let mut crc = size ^ cmd;
    for &b in payload {
        crc ^= b;
    }
    out.push(crc);
    Some(out)
}

/// Encode an MSPv2 request `$X<` with `flag = 0` (the common case).
pub fn encode_v2(cmd: u16, payload: &[u8]) -> Vec<u8> {
    encode_v2_flagged(0, cmd, payload)
}

/// Encode an MSPv2 request `$X<` with an explicit flag byte.
pub fn encode_v2_flagged(flag: u8, cmd: u16, payload: &[u8]) -> Vec<u8> {
    let size = payload.len() as u16;
    let mut out = Vec::with_capacity(9 + payload.len());
    out.extend_from_slice(&[b'$', b'X', DIR_TO_FC]);
    // The CRC covers flag, cmd (LE), size (LE), and the payload — everything
    // after the direction byte except the CRC itself.
    let mut header = Vec::with_capacity(5);
    header.push(flag);
    header.extend_from_slice(&cmd.to_le_bytes());
    header.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(payload);
    let mut crc = crc8_dvb_s2_buf(&header);
    for &b in payload {
        crc = crc8_dvb_s2(crc, b);
    }
    out.push(crc);
    out
}

/// A decoded MSP frame: its command id, direction/marker byte, and payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MspFrame {
    pub cmd: u16,
    /// The direction byte: [`DIR_TO_FC`], [`DIR_FROM_FC`], or [`DIR_ERROR`].
    pub dir: u8,
    pub payload: Vec<u8>,
    /// The MSP version the frame was encoded in (1 or 2).
    pub version: u8,
}

/// Why a byte buffer could not be decoded as a single complete MSP frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MspDecodeError {
    /// Fewer bytes than the smallest valid frame of its kind.
    TooShort,
    /// The `$M` / `$X` preamble or the version byte was not recognized.
    BadPreamble,
    /// The trailing CRC did not match the computed CRC (corrupt frame).
    BadCrc { expected: u8, found: u8 },
}

/// Decode exactly one MSP frame from the START of `buf` (v1 or v2, auto-detected
/// from the `$M` / `$X` preamble). Returns the frame and the number of bytes it
/// consumed, so a caller streaming from the socket can advance past it. Verifies
/// the CRC and fails closed on a mismatch — a corrupt FC reply must never be
/// handed on as if it were valid (Rule 44: a status surface reports verified
/// data).
pub fn decode_frame(buf: &[u8]) -> Result<(MspFrame, usize), MspDecodeError> {
    if buf.len() < 3 {
        return Err(MspDecodeError::TooShort);
    }
    if buf[0] != b'$' {
        return Err(MspDecodeError::BadPreamble);
    }
    match buf[1] {
        b'M' => decode_v1(buf),
        b'X' => decode_v2(buf),
        _ => Err(MspDecodeError::BadPreamble),
    }
}

fn decode_v1(buf: &[u8]) -> Result<(MspFrame, usize), MspDecodeError> {
    // '$' 'M' dir size cmd payload... crc  -> min 6 bytes (empty payload)
    if buf.len() < 6 {
        return Err(MspDecodeError::TooShort);
    }
    let dir = buf[2];
    let size = buf[3] as usize;
    let cmd = buf[4] as u16;
    let total = 6 + size;
    if buf.len() < total {
        return Err(MspDecodeError::TooShort);
    }
    let payload = &buf[5..5 + size];
    let mut crc = buf[3] ^ buf[4];
    for &b in payload {
        crc ^= b;
    }
    let found = buf[total - 1];
    if crc != found {
        return Err(MspDecodeError::BadCrc {
            expected: crc,
            found,
        });
    }
    Ok((
        MspFrame {
            cmd,
            dir,
            payload: payload.to_vec(),
            version: 1,
        },
        total,
    ))
}

fn decode_v2(buf: &[u8]) -> Result<(MspFrame, usize), MspDecodeError> {
    // '$' 'X' dir flag cmd_lo cmd_hi size_lo size_hi payload... crc -> min 9
    if buf.len() < 9 {
        return Err(MspDecodeError::TooShort);
    }
    let dir = buf[2];
    let flag = buf[3];
    let cmd = u16::from_le_bytes([buf[4], buf[5]]);
    let size = u16::from_le_bytes([buf[6], buf[7]]) as usize;
    let total = 9 + size;
    if buf.len() < total {
        return Err(MspDecodeError::TooShort);
    }
    let payload = &buf[8..8 + size];
    let mut crc = crc8_dvb_s2_buf(&buf[3..8]); // flag + cmd + size
    let _ = flag; // documented; covered by the header slice above
    for &b in payload {
        crc = crc8_dvb_s2(crc, b);
    }
    let found = buf[total - 1];
    if crc != found {
        return Err(MspDecodeError::BadCrc {
            expected: crc,
            found,
        });
    }
    Ok((
        MspFrame {
            cmd,
            dir,
            payload: payload.to_vec(),
            version: 2,
        },
        total,
    ))
}

/// Map a bipolar stick axis (roll / pitch / yaw), `-1.0..=1.0`, to a PWM value
/// `1000..=2000` with centre `1500`. Out-of-range inputs clamp. This matches the
/// GCS `bipolarToPwm` mapping so a companion and the GCS scale sticks identically.
pub fn bipolar_to_pwm(v: f32) -> u16 {
    let clamped = v.clamp(-1.0, 1.0);
    (1500.0 + clamped * 500.0).round() as u16
}

/// Map a throttle input, `0.0..=1.0`, to a PWM value `1000..=2000` with idle at
/// `1000` (NOT centre — a throttle at rest is idle, not mid-stick). Matches the
/// GCS `throttleToPwm` mapping.
pub fn throttle_to_pwm(t: f32) -> u16 {
    let clamped = t.clamp(0.0, 1.0);
    (1000.0 + clamped * 1000.0).round() as u16
}

/// Encode an `MSP_SET_RAW_RC` command carrying `channels` PWM microsecond values
/// as little-endian `u16`s. `v2` selects the framing (v2 is the safe default for
/// a modern FC; v1 is offered for a legacy target). A v1 encode of more than 127
/// channels overflows the byte size field and returns `None`.
pub fn set_raw_rc(channels: &[u16], v2: bool) -> Option<Vec<u8>> {
    let mut payload = Vec::with_capacity(channels.len() * 2);
    for &ch in channels {
        payload.extend_from_slice(&ch.to_le_bytes());
    }
    if v2 {
        Some(encode_v2(MSP_SET_RAW_RC, &payload))
    } else {
        encode_v1(MSP_SET_RAW_RC, &payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Golden byte vectors: the codec is the interop boundary, so pin the
    // exact bytes rather than only round-tripping (a symmetric bug in encode +
    // decode would pass a round-trip and still be wrong on the wire).

    #[test]
    fn crc8_dvb_s2_matches_known_values() {
        // DVB-S2 CRC8 of a single 0x00 is 0x00; of {0x00,0x00} is 0x00.
        assert_eq!(crc8_dvb_s2_buf(&[0x00]), 0x00);
        // A single 0x01 through the poly 0xD5.
        assert_eq!(crc8_dvb_s2_buf(&[0x01]), 0xD5);
    }

    #[test]
    fn v1_msp_status_request_golden() {
        // MSP_STATUS (101) with no payload: $ M < 00 65 crc, crc = 0x00 ^ 0x65.
        let frame = encode_v1(101, &[]).unwrap();
        assert_eq!(frame, vec![b'$', b'M', b'<', 0x00, 0x65, 0x65]);
    }

    #[test]
    fn v1_with_payload_golden() {
        // cmd 200, payload [0xE8,0x03] (=1000 LE). size=2.
        // crc = 2 ^ 200 ^ 0xE8 ^ 0x03.
        let frame = encode_v1(200, &[0xE8, 0x03]).unwrap();
        let crc = 2u8 ^ 200 ^ 0xE8 ^ 0x03;
        assert_eq!(frame, vec![b'$', b'M', b'<', 0x02, 0xC8, 0xE8, 0x03, crc]);
    }

    #[test]
    fn v1_rejects_oversize() {
        assert!(encode_v1(256, &[]).is_none()); // cmd > 255
        assert!(encode_v1(1, &vec![0u8; 256]).is_none()); // payload > 255
    }

    #[test]
    fn v2_msp_status_request_golden() {
        // $ X < flag=00 cmd=65 00 size=00 00 crc. The CRC 0xCA is from an
        // INDEPENDENT CRC8/DVB-S2 oracle (verified against the canonical
        // "123456789" -> 0xBC check value), not this module's own function, so a
        // symmetric encode/crc bug cannot pass.
        let frame = encode_v2(101, &[]);
        assert_eq!(
            frame,
            vec![b'$', b'X', b'<', 0x00, 0x65, 0x00, 0x00, 0x00, 0xCA]
        );
    }

    #[test]
    fn set_raw_rc_v2_golden() {
        // Two channels at 1500 (0xDC 0x05) each. cmd=200 (0xC8 0x00), size=4.
        // Full frame incl. the CRC 0xB4 from the independent oracle above.
        let bytes = set_raw_rc(&[1500, 1500], true).unwrap();
        assert_eq!(
            bytes,
            vec![b'$', b'X', b'<', 0x00, 0xC8, 0x00, 0x04, 0x00, 0xDC, 0x05, 0xDC, 0x05, 0xB4]
        );
    }

    #[test]
    fn decode_roundtrips_v1_and_v2() {
        for &v2 in &[false, true] {
            let bytes = set_raw_rc(&[1000, 1500, 2000, 1500], v2).unwrap();
            let (frame, consumed) = decode_frame(&bytes).unwrap();
            assert_eq!(consumed, bytes.len());
            assert_eq!(frame.cmd, MSP_SET_RAW_RC);
            assert_eq!(frame.dir, DIR_TO_FC);
            assert_eq!(frame.version, if v2 { 2 } else { 1 });
            // Payload decodes back to the channel values.
            let chans: Vec<u16> = frame
                .payload
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            assert_eq!(chans, vec![1000, 1500, 2000, 1500]);
        }
    }

    #[test]
    fn decode_rejects_a_corrupt_crc() {
        let mut bytes = set_raw_rc(&[1500], true).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF; // corrupt the CRC
        assert!(matches!(
            decode_frame(&bytes),
            Err(MspDecodeError::BadCrc { .. })
        ));
        // v1 too.
        let mut v1 = set_raw_rc(&[1500], false).unwrap();
        let l = v1.len() - 1;
        v1[l] ^= 0xFF;
        assert!(matches!(
            decode_frame(&v1),
            Err(MspDecodeError::BadCrc { .. })
        ));
    }

    #[test]
    fn decode_reports_short_and_bad_preamble() {
        assert_eq!(decode_frame(b"$"), Err(MspDecodeError::TooShort));
        assert_eq!(
            decode_frame(&[b'$', b'Z', b'<', 0, 0, 0]),
            Err(MspDecodeError::BadPreamble)
        );
        // A v2 frame missing its payload+crc is short, not corrupt.
        assert_eq!(
            decode_frame(&[b'$', b'X', b'<', 0, 0xC8, 0, 4, 0]),
            Err(MspDecodeError::TooShort)
        );
    }

    #[test]
    fn stick_scaling_matches_the_gcs() {
        // Bipolar: centre, extremes, clamp.
        assert_eq!(bipolar_to_pwm(0.0), 1500);
        assert_eq!(bipolar_to_pwm(1.0), 2000);
        assert_eq!(bipolar_to_pwm(-1.0), 1000);
        assert_eq!(bipolar_to_pwm(2.0), 2000); // clamp high
        assert_eq!(bipolar_to_pwm(-2.0), 1000); // clamp low
        assert_eq!(bipolar_to_pwm(0.5), 1750);
        // Throttle: idle at 0, NOT centre. A throttle scaled to mid-stick would
        // arm a motor at half power the instant the channel opened.
        assert_eq!(throttle_to_pwm(0.0), 1000);
        assert_eq!(throttle_to_pwm(1.0), 2000);
        assert_eq!(throttle_to_pwm(0.5), 1500);
        assert_eq!(throttle_to_pwm(-0.1), 1000); // clamp
        assert_eq!(throttle_to_pwm(1.1), 2000); // clamp
    }
}
