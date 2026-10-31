//! Numeric helpers shared by the setpoint encoders.

use super::*;

/// Clamp an f64 into the i32 range without wrapping (used for the global
/// message's scaled lat/lon). A non-finite value is validated out before this is
/// reached on an active axis; an ignored-axis non-finite value clamps to 0.
pub(super) fn clamp_to_i32(v: f64) -> i32 {
    if v.is_nan() {
        return 0;
    }
    v.clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

/// Map a numeric coordinate-frame id to the dialect enum. An id outside the
/// validated frame sets never reaches here (the builder validates first), so an
/// unexpected value falls back to the enum default rather than panicking.
///
/// `#[allow(deprecated)]`: the dialect marks MAV_FRAME_GLOBAL_INT,
/// _GLOBAL_RELATIVE_ALT_INT, _LOCAL_OFFSET_NED, _BODY_NED and _BODY_OFFSET_NED
/// superseded, but supersession is a spec-authoring statement, not a wire
/// change: ids 5..=9 are still what a flight controller puts on the link, and
/// dropping the arms would silently map live frames onto the enum default.
#[allow(deprecated)]
pub(super) fn mav_frame_from_u8(frame: u8) -> ardupilotmega::MavFrame {
    use ardupilotmega::MavFrame::*;
    match frame {
        0 => MAV_FRAME_GLOBAL,
        1 => MAV_FRAME_LOCAL_NED,
        3 => MAV_FRAME_GLOBAL_RELATIVE_ALT,
        5 => MAV_FRAME_GLOBAL_INT,
        6 => MAV_FRAME_GLOBAL_RELATIVE_ALT_INT,
        7 => MAV_FRAME_LOCAL_OFFSET_NED,
        8 => MAV_FRAME_BODY_NED,
        9 => MAV_FRAME_BODY_OFFSET_NED,
        12 => MAV_FRAME_BODY_FRD,
        _ => ardupilotmega::MavFrame::DEFAULT,
    }
}

// ---------------------------------------------------------------------------
// SET_ATTITUDE_TARGET attitude setpoint builder (message id 82).
// ---------------------------------------------------------------------------
