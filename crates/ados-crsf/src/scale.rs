//! Input scaling: evdev-style axis and switch values → CRSF channel values.
//!
//! Pure functions, host-testable with zero hardware. The device read loop that
//! produces the raw values lives elsewhere; this module owns only the maths.

use crate::channels::{CHANNEL_MAX, CHANNEL_MID, CHANNEL_MIN};

/// Full-scale evdev axis minimum (`ABS_X`-style signed 16-bit).
pub const AXIS_MIN: i32 = -32768;
/// Full-scale evdev axis maximum.
pub const AXIS_MAX: i32 = 32767;

/// Scale a signed full-range axis value onto the CRSF channel range.
///
/// `AXIS_MIN` maps to 172 (full low), `AXIS_MAX` maps to 1811 (full high),
/// and 0 maps to the 992 center (the positive half-range is one count
/// shorter, so the two halves scale independently around the exact center —
/// a centered stick must read exactly 992, not 991 or 993). Out-of-range
/// input clamps to the endpoints.
pub fn axis_to_channel(raw: i32) -> u16 {
    let raw = raw.clamp(AXIS_MIN, AXIS_MAX);
    if raw == 0 {
        return CHANNEL_MID;
    }
    if raw < 0 {
        // -32768..0 → 172..992 over a span of 32768 counts / 820 steps.
        let span = i64::from(CHANNEL_MID - CHANNEL_MIN); // 820
        let offset = i64::from(raw) - i64::from(AXIS_MIN); // 0..32768
        (i64::from(CHANNEL_MIN) + (offset * span + 16384) / 32768) as u16
    } else {
        // 0..32767 → 992..1811 over a span of 32767 counts / 819 steps.
        let span = i64::from(CHANNEL_MAX - CHANNEL_MID); // 819
        (i64::from(CHANNEL_MID) + (i64::from(raw) * span + 16383) / 32767) as u16
    }
}

/// Map a discrete switch position onto the CRSF channel range: position `pos`
/// of a switch with `positions` detents (2-position → 172/1811, 3-position →
/// 172/992/1811, n-position → evenly spaced). `positions` below 2 is treated
/// as a 2-position switch; `pos` beyond the last detent clamps to it.
pub fn switch_to_channel(pos: u8, positions: u8) -> u16 {
    let positions = positions.max(2);
    let pos = pos.min(positions - 1);
    let span = i64::from(CHANNEL_MAX - CHANNEL_MIN);
    let steps = i64::from(positions - 1);
    (i64::from(CHANNEL_MIN) + (i64::from(pos) * span + steps / 2) / steps) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axis_endpoints_hit_the_channel_endpoints() {
        assert_eq!(axis_to_channel(AXIS_MIN), CHANNEL_MIN);
        assert_eq!(axis_to_channel(AXIS_MAX), CHANNEL_MAX);
        assert_eq!(axis_to_channel(0), CHANNEL_MID);
    }

    #[test]
    fn axis_out_of_range_clamps() {
        assert_eq!(axis_to_channel(i32::MIN), CHANNEL_MIN);
        assert_eq!(axis_to_channel(i32::MAX), CHANNEL_MAX);
    }

    #[test]
    fn axis_scaling_is_monotonic_and_in_range() {
        let mut prev = axis_to_channel(AXIS_MIN);
        for raw in (AXIS_MIN..=AXIS_MAX).step_by(997) {
            let ch = axis_to_channel(raw);
            assert!((CHANNEL_MIN..=CHANNEL_MAX).contains(&ch));
            assert!(ch >= prev, "scaling must be monotonic");
            prev = ch;
        }
    }

    #[test]
    fn axis_half_scale_lands_near_quarter_points() {
        // Half deflection each way lands halfway through its half-range
        // (rounded), per the two-half scaling rule.
        let low_half = axis_to_channel(AXIS_MIN / 2);
        assert!((581..=583).contains(&low_half), "got {low_half}");
        let high_half = axis_to_channel(AXIS_MAX / 2);
        assert!((1401..=1403).contains(&high_half), "got {high_half}");
    }

    #[test]
    fn two_position_switch_maps_to_endpoints() {
        assert_eq!(switch_to_channel(0, 2), CHANNEL_MIN);
        assert_eq!(switch_to_channel(1, 2), CHANNEL_MAX);
    }

    #[test]
    fn three_position_switch_maps_low_center_high() {
        assert_eq!(switch_to_channel(0, 3), CHANNEL_MIN);
        // Midpoint of 172..1811 rounds to 992 exactly (1639 / 2 = 819.5 → 820).
        assert_eq!(switch_to_channel(1, 3), CHANNEL_MID);
        assert_eq!(switch_to_channel(2, 3), CHANNEL_MAX);
    }

    #[test]
    fn switch_degenerate_inputs_clamp() {
        // Zero/one-position switches degrade to 2-position; positions beyond
        // the last detent clamp to full high.
        assert_eq!(switch_to_channel(0, 0), CHANNEL_MIN);
        assert_eq!(switch_to_channel(1, 1), CHANNEL_MAX);
        assert_eq!(switch_to_channel(9, 3), CHANNEL_MAX);
    }

    #[test]
    fn six_position_switch_is_evenly_spaced_and_monotonic() {
        let values: Vec<u16> = (0..6).map(|p| switch_to_channel(p, 6)).collect();
        assert_eq!(values[0], CHANNEL_MIN);
        assert_eq!(values[5], CHANNEL_MAX);
        for w in values.windows(2) {
            let step = w[1] - w[0];
            // 1639 / 5 = 327.8 → each step is 327 or 328.
            assert!((327..=328).contains(&step), "step {step}");
        }
    }
}
