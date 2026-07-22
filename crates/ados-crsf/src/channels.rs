//! RC channel packing: 16 channels × 11 bits, little-endian LSB-first, the
//! payload of the `RC_CHANNELS_PACKED` (0x16) frame.
//!
//! Channel value convention: `172` = full low, `992` = center, `1811` = full
//! high. The 11 bits could carry 0..2047 but the usable endpoint range is
//! 172..=1811 (1640 steps); values this service transmits are clamped to that
//! range at the injection boundary, never here (the codec is faithful to the
//! wire and round-trips any 11-bit value).

use crate::frame::{build_frame, FrameError, ADDR_FLIGHT_CONTROLLER, TYPE_RC_CHANNELS_PACKED};

/// Number of RC channels in the packed frame.
pub const CHANNEL_COUNT: usize = 16;
/// Packed payload size: 16 × 11 bits = 176 bits = 22 bytes.
pub const PACKED_SIZE: usize = 22;
/// Full-low endpoint value.
pub const CHANNEL_MIN: u16 = 172;
/// Center value.
pub const CHANNEL_MID: u16 = 992;
/// Full-high endpoint value.
pub const CHANNEL_MAX: u16 = 1811;
/// Hard 11-bit ceiling a packed channel can physically carry.
pub const CHANNEL_RAW_MAX: u16 = 0x7FF;

/// Pack 16 channels into the 22-byte wire payload. Each channel contributes
/// its low 11 bits, accumulated LSB-first into a little-endian bit stream
/// (channel 0 occupies payload bits 0..10, channel 1 bits 11..21, …).
pub fn pack_channels(channels: &[u16; CHANNEL_COUNT]) -> [u8; PACKED_SIZE] {
    let mut out = [0u8; PACKED_SIZE];
    let mut bits: u32 = 0;
    let mut bit_count: u8 = 0;
    let mut byte_idx = 0;
    for &ch in channels {
        bits |= u32::from(ch & CHANNEL_RAW_MAX) << bit_count;
        bit_count += 11;
        while bit_count >= 8 {
            out[byte_idx] = (bits & 0xFF) as u8;
            byte_idx += 1;
            bits >>= 8;
            bit_count -= 8;
        }
    }
    out
}

/// Unpack the 22-byte payload back into 16 channel values.
///
/// Deliberately implemented by per-bit indexing (channel `ch` bit `b` is
/// payload bit `ch * 11 + b`), NOT by reversing the packer's accumulator —
/// two independent readings of the bit-layout rule, so the round-trip tests
/// genuinely cross-check the packer instead of mirroring it.
pub fn unpack_channels(payload: &[u8; PACKED_SIZE]) -> [u16; CHANNEL_COUNT] {
    let mut out = [0u16; CHANNEL_COUNT];
    for (ch, slot) in out.iter_mut().enumerate() {
        let mut value: u16 = 0;
        for b in 0..11 {
            let bit_index = ch * 11 + b;
            let bit = (payload[bit_index / 8] >> (bit_index % 8)) & 1;
            value |= u16::from(bit) << b;
        }
        *slot = value;
    }
    out
}

/// Build the complete 26-byte RC channels wire frame:
/// `[0xC8, 0x18, 0x16, 22-byte payload, crc]`.
pub fn build_rc_frame(channels: &[u16; CHANNEL_COUNT]) -> Result<Vec<u8>, FrameError> {
    build_frame(
        ADDR_FLIGHT_CONTROLLER,
        TYPE_RC_CHANNELS_PACKED,
        &pack_channels(channels),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::parse_frame;

    /// The 11-byte pattern eight center-value (992 = 0x3E0) channels pack to,
    /// derived by hand from the LSB-first layout: byte 0 carries ch0 bits 0-7
    /// (0xE0), the three leftover ch0 bits (0b011) land in byte 1's low bits
    /// under ch1's low five bits, and so on; eight 11-bit channels realign on
    /// a byte boundary every 88 bits, so 16 equal channels repeat the pattern
    /// twice.
    const CENTER_UNIT: [u8; 11] = [
        0xE0, 0x03, 0x1F, 0xF8, 0xC0, 0x07, 0x3E, 0xF0, 0x81, 0x0F, 0x7C,
    ];
    /// Same derivation for the full-low endpoint 172 = 0x0AC.
    const LOW_UNIT: [u8; 11] = [
        0xAC, 0x60, 0x05, 0x2B, 0x58, 0xC1, 0x0A, 0x56, 0xB0, 0x82, 0x15,
    ];
    /// Same derivation for the full-high endpoint 1811 = 0x713.
    const HIGH_UNIT: [u8; 11] = [
        0x13, 0x9F, 0xF8, 0xC4, 0x27, 0x3E, 0xF1, 0x89, 0x4F, 0x7C, 0xE2,
    ];

    fn doubled(unit: &[u8; 11]) -> [u8; PACKED_SIZE] {
        let mut out = [0u8; PACKED_SIZE];
        out[..11].copy_from_slice(unit);
        out[11..].copy_from_slice(unit);
        out
    }

    #[test]
    fn packs_all_center_to_the_derived_pattern() {
        assert_eq!(pack_channels(&[CHANNEL_MID; 16]), doubled(&CENTER_UNIT));
    }

    #[test]
    fn packs_all_low_to_the_derived_pattern() {
        let packed = pack_channels(&[CHANNEL_MIN; 16]);
        // Byte 0 of an all-172 payload is the low eight bits of 172 = 0xAC.
        assert_eq!(packed[0], 0xAC);
        assert_eq!(packed, doubled(&LOW_UNIT));
    }

    #[test]
    fn packs_all_high_to_the_derived_pattern() {
        assert_eq!(pack_channels(&[CHANNEL_MAX; 16]), doubled(&HIGH_UNIT));
    }

    #[test]
    fn unpack_reads_the_derived_patterns_back() {
        // The independent bit-indexing reader agrees with the hand-derived
        // byte patterns, closing the loop on both implementations.
        assert_eq!(unpack_channels(&doubled(&CENTER_UNIT)), [CHANNEL_MID; 16]);
        assert_eq!(unpack_channels(&doubled(&LOW_UNIT)), [CHANNEL_MIN; 16]);
        assert_eq!(unpack_channels(&doubled(&HIGH_UNIT)), [CHANNEL_MAX; 16]);
    }

    #[test]
    fn roundtrip_endpoint_and_distinct_values() {
        let cases: [[u16; 16]; 4] = [
            [CHANNEL_MIN; 16],
            [CHANNEL_MID; 16],
            [CHANNEL_MAX; 16],
            [
                172, 992, 1811, 500, 1000, 1500, 300, 700, 1100, 1300, 900, 600, 400, 1700, 200,
                2047,
            ],
        ];
        for case in cases {
            assert_eq!(unpack_channels(&pack_channels(&case)), case);
        }
    }

    #[test]
    fn roundtrip_pseudo_random_11_bit_vectors() {
        // Deterministic pseudo-random 11-bit values (linear congruential walk);
        // every one must survive the pack/unpack round trip.
        let mut seed: u32 = 0x1234_5678;
        for _ in 0..200 {
            let mut chans = [0u16; 16];
            for slot in chans.iter_mut() {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *slot = (seed >> 16) as u16 & CHANNEL_RAW_MAX;
            }
            assert_eq!(unpack_channels(&pack_channels(&chans)), chans);
        }
    }

    #[test]
    fn pack_masks_out_of_range_input_to_11_bits() {
        // The codec is faithful to the wire: only the low 11 bits travel.
        let mut chans = [CHANNEL_MID; 16];
        chans[0] = 0xFFFF;
        assert_eq!(unpack_channels(&pack_channels(&chans))[0], 0x7FF);
    }

    #[test]
    fn rc_frame_is_26_bytes_with_the_documented_header() {
        let wire = build_rc_frame(&[CHANNEL_MID; 16]).unwrap();
        assert_eq!(wire.len(), 26);
        assert_eq!(wire[0], 0xC8, "device address");
        assert_eq!(wire[1], 0x18, "length byte: type + 22 payload + crc = 24");
        assert_eq!(wire[2], 0x16, "RC channels frame type");
        let (frame, consumed) = parse_frame(&wire).unwrap();
        assert_eq!(consumed, 26);
        let payload: [u8; PACKED_SIZE] = frame.payload.try_into().unwrap();
        assert_eq!(unpack_channels(&payload), [CHANNEL_MID; 16]);
    }
}
