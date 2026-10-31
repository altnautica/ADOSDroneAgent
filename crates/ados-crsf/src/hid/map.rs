//! Stick and switch maps and the live channel frame.

use super::*;

/// One absolute-axis mapping: an evdev ABS code onto a channel index.
#[derive(Debug, Clone, Copy)]
pub struct AxisMap {
    pub code: u16,
    pub channel: usize,
    pub inverted: bool,
}

/// One key/button mapping: an evdev KEY code onto an aux channel index.
#[derive(Debug, Clone, Copy)]
pub struct ButtonMap {
    pub code: u16,
    pub channel: usize,
}

/// The default stick map (evdev ABS codes; mode-2 AETR channel order).
pub const DEFAULT_AXIS_MAP: [AxisMap; 4] = [
    // ABS_RX → roll.
    AxisMap {
        code: 0x03,
        channel: 0,
        inverted: false,
    },
    // ABS_RY → pitch (stick forward = nose down = channel low on the wire's
    // convention; evdev Y grows downward, so forward is negative → invert).
    AxisMap {
        code: 0x04,
        channel: 1,
        inverted: true,
    },
    // ABS_Y → throttle (evdev Y grows downward; stick up = more throttle).
    AxisMap {
        code: 0x01,
        channel: 2,
        inverted: true,
    },
    // ABS_X → yaw.
    AxisMap {
        code: 0x00,
        channel: 3,
        inverted: false,
    },
];

/// The default switch map: the eight face/shoulder buttons onto aux channels
/// 4..=11 as two-position switches (evdev BTN_SOUTH..BTN_START codes).
pub const DEFAULT_BUTTON_MAP: [ButtonMap; 8] = [
    ButtonMap {
        code: 0x130, // BTN_SOUTH
        channel: 4,
    },
    ButtonMap {
        code: 0x131, // BTN_EAST
        channel: 5,
    },
    ButtonMap {
        code: 0x133, // BTN_NORTH
        channel: 6,
    },
    ButtonMap {
        code: 0x134, // BTN_WEST
        channel: 7,
    },
    ButtonMap {
        code: 0x136, // BTN_TL
        channel: 8,
    },
    ButtonMap {
        code: 0x137, // BTN_TR
        channel: 9,
    },
    ButtonMap {
        code: 0x13A, // BTN_SELECT
        channel: 10,
    },
    ButtonMap {
        code: 0x13B, // BTN_START
        channel: 11,
    },
];

/// A per-axis calibration read from the device's absinfo at open time.
#[derive(Debug, Clone, Copy)]
pub struct AxisCal {
    pub code: u16,
    pub min: i32,
    pub max: i32,
}

/// Scale a raw absolute-axis sample with its device range onto a CRSF channel
/// value: `min` → 172, `max` → 1811, the range midpoint → ~992 (exact when
/// the range is symmetric around zero). Degenerate ranges read center.
pub fn abs_to_channel(raw: i32, min: i32, max: i32, inverted: bool) -> u16 {
    if max <= min {
        return CHANNEL_MID;
    }
    let span = i64::from(max) - i64::from(min);
    let offset = i64::from(raw.clamp(min, max)) - i64::from(min);
    // Onto the full signed evdev-style range, then through the shared
    // two-half scaler so every axis uses the same center-exact maths.
    let mut scaled = (offset * 65_535 / span) - 32_768;
    if inverted {
        scaled = -scaled;
    }
    axis_to_channel(scaled.clamp(-32_768, 32_767) as i32)
}

/// The HID source's live channel frame: starts from the default bank, then
/// follows the device's polled state and events.
#[derive(Debug, Clone)]
pub struct HidChannels {
    values: [u16; CHANNEL_COUNT],
}

impl Default for HidChannels {
    fn default() -> Self {
        Self {
            values: ados_crsf_codec::bank::ChannelBank::default().values(),
        }
    }
}

impl HidChannels {
    pub fn values(&self) -> [u16; CHANNEL_COUNT] {
        self.values
    }

    /// Apply an absolute-axis sample through the default map + the device
    /// calibration. Returns whether any channel changed.
    pub fn apply_abs(&mut self, code: u16, raw: i32, cal: &[AxisCal]) -> bool {
        let Some(map) = DEFAULT_AXIS_MAP.iter().find(|m| m.code == code) else {
            return false;
        };
        let Some(cal) = cal.iter().find(|c| c.code == code) else {
            return false;
        };
        let value = abs_to_channel(raw, cal.min, cal.max, map.inverted);
        if self.values[map.channel] == value {
            return false;
        }
        self.values[map.channel] = value;
        true
    }

    /// Apply a key/button edge through the default switch map. Returns whether
    /// any channel changed.
    pub fn apply_key(&mut self, code: u16, pressed: bool) -> bool {
        let Some(map) = DEFAULT_BUTTON_MAP.iter().find(|m| m.code == code) else {
            return false;
        };
        let value = switch_to_channel(u8::from(pressed), 2);
        if self.values[map.channel] == value {
            return false;
        }
        self.values[map.channel] = value;
        true
    }

    /// Apply one stream observation. Returns whether any channel changed.
    pub fn apply_input(&mut self, input: HidInput, cal: &[AxisCal]) -> bool {
        match input {
            HidInput::Abs { code, value } => self.apply_abs(code, value, cal),
            HidInput::Key { code, pressed } => self.apply_key(code, pressed),
            HidInput::Other => false,
        }
    }

    /// Apply a full polled device state. Axes without a calibration (the
    /// device does not report them) keep their current value.
    pub fn apply_snapshot(&mut self, snapshot: &HidSnapshot, cal: &[AxisCal]) {
        for (map, &raw) in DEFAULT_AXIS_MAP.iter().zip(snapshot.axes.iter()) {
            self.apply_abs(map.code, raw, cal);
        }
        for (map, &pressed) in DEFAULT_BUTTON_MAP.iter().zip(snapshot.buttons.iter()) {
            self.apply_key(map.code, pressed);
        }
    }
}
