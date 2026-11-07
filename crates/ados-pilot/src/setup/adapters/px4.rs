//! PX4: step lists, main/sub mode numbers, calibration text protocol.

use super::Mode;

pub const GROUND: &[&str] = &[
    "detect", "frame", "accel", "level", "gyro", "compass", "radio", "outputs", "battery", "ados_profile", "ados_intro",
];

pub const AIR: &[&str] = &["preflight", "first_hover", "autotune", "verify"];

// PX4 `px4_custom_mode` main modes and AUTO sub modes.
const MAIN_POSCTL: u32 = 3;
const MAIN_AUTO: u32 = 4;
const MAIN_OFFBOARD: u32 = 6;
const SUB_TAKEOFF: u32 = 2;
const SUB_LOITER: u32 = 3;
const SUB_RTL: u32 = 5;
const SUB_LAND: u32 = 6;

pub fn mode(m: Mode) -> Option<(u32, u32)> {
    Some(match m {
        Mode::Offboard | Mode::Guided => (MAIN_OFFBOARD, 0),
        Mode::Posctl => (MAIN_POSCTL, 0),
        Mode::Land => (MAIN_AUTO, SUB_LAND),
        Mode::Rtl => (MAIN_AUTO, SUB_RTL),
        Mode::Loiter => (MAIN_AUTO, SUB_LOITER),
        Mode::Takeoff => (MAIN_AUTO, SUB_TAKEOFF),
        _ => return None,
    })
}

/// `(main, sub)` from a heartbeat's `custom_mode`.
pub fn decode(custom_mode: u32) -> (u32, u32) {
    let main = (custom_mode >> 16) & 0xff;
    let sub = (custom_mode >> 24) & 0xff;
    // Only AUTO carries a meaningful sub mode.
    (main, if main == MAIN_AUTO { sub } else { 0 })
}

/// PX4 `[cal]` STATUSTEXT protocol events.
#[derive(Debug, Clone, PartialEq)]
pub enum CalEvent {
    Started,
    /// Sides still to do (`up down left right front back`).
    Pending(Vec<String>),
    /// A side was detected and is being sampled.
    Detected(String),
    SideDone(String),
    Progress(u8),
    Done,
    Failed(String),
}

/// Parses one PX4 calibration STATUSTEXT (`[cal] …`).
pub fn parse_cal(text: &str) -> Option<CalEvent> {
    let t = text.strip_prefix("[cal] ")?.trim();
    let lower = t.to_ascii_lowercase();
    if lower.starts_with("calibration started") {
        return Some(CalEvent::Started);
    }
    if lower.starts_with("calibration done") {
        return Some(CalEvent::Done);
    }
    if lower.starts_with("calibration failed") || lower.starts_with("calibration cancelled") {
        return Some(CalEvent::Failed(t.to_string()));
    }
    if let Some(rest) = lower.strip_prefix("pending:") {
        return Some(CalEvent::Pending(rest.split_whitespace().map(str::to_string).collect()));
    }
    if let Some(rest) = lower.strip_prefix("progress ") {
        return rest.trim_end_matches('%').trim().parse::<u8>().ok().map(CalEvent::Progress);
    }
    if let Some(side) = lower.strip_suffix(" orientation detected") {
        return Some(CalEvent::Detected(side.to_string()));
    }
    if let Some(idx) = lower.find(" side done") {
        return Some(CalEvent::SideDone(lower[..idx].to_string()));
    }
    None
}
