//! ArduPlane (and QuadPlane VTOL): step lists, modes, control surfaces.

use super::Mode;

pub const GROUND: &[&str] = &[
    "detect", "frame", "accel", "level", "compass", "airspeed", "radio", "rc_ados_switch", "outputs",
    "battery", "initial_params", "ados_profile", "ados_intro",
];

pub const GROUND_VTOL: &[&str] = &[
    "detect", "frame", "accel", "level", "compass", "airspeed", "radio", "rc_ados_switch", "outputs",
    "battery", "initial_params", "notch_static", "ados_profile", "ados_intro",
];

pub const AIR: &[&str] = &["preflight", "first_flight", "trim_air", "airspeed_cal", "tecs_measure", "autotune", "verify"];

pub const AIR_VTOL: &[&str] = &[
    "preflight", "first_hover", "notch_air", "trim_air", "airspeed_cal", "tecs_measure", "autotune", "verify",
];

pub fn mode(m: Mode) -> Option<u32> {
    Some(match m {
        Mode::Manual => 0,
        Mode::Fbwa => 5,
        Mode::Fbwb => 6,
        Mode::Autotune => 8,
        Mode::Rtl => 11,
        Mode::Loiter => 12,
        Mode::Takeoff => 13,
        Mode::Guided => 15,
        Mode::Qhover => 18,
        Mode::Qloiter => 19,
        Mode::Land => 20, // QLAND: only meaningful on a VTOL
        Mode::Qautotune => 22,
        _ => return None,
    })
}

/// Output functions of the control surfaces (`SERVOn_FUNCTION`).
pub const FN_AILERON: u8 = 4;
pub const FN_ELEVATOR: u8 = 19;
pub const FN_RUDDER: u8 = 21;
pub const FN_THROTTLE: u8 = 70;
pub const FN_ELEVON_LEFT: u8 = 77;
pub const FN_ELEVON_RIGHT: u8 = 78;
pub const FN_VTAIL_LEFT: u8 = 79;
pub const FN_VTAIL_RIGHT: u8 = 80;

/// Output presets per wing type, on SERVO1..4.
pub fn wing_preset(wing: &str) -> Option<&'static [(u8, u8)]> {
    Some(match wing {
        "normal" => &[(1, FN_AILERON), (2, FN_ELEVATOR), (3, FN_THROTTLE), (4, FN_RUDDER)],
        "elevon" => &[(1, FN_ELEVON_LEFT), (2, FN_ELEVON_RIGHT), (3, FN_THROTTLE)],
        "vtail" => &[(1, FN_AILERON), (2, FN_VTAIL_LEFT), (3, FN_THROTTLE), (4, FN_VTAIL_RIGHT)],
        _ => return None,
    })
}

/// The surface test for an output function: the question key the operator
/// answers, and the sign of the scaled output that the tested demand
/// produces on a non-reversed servo. ArduPlane's mixer
/// (`channel_function_mixer`) makes a non-reversed elevon or V-tail output
/// move the surface up for a positive pitch demand; ailerons are tested
/// with a right roll, the rudder with a right yaw.
pub fn surface_test(function: u8) -> Option<(&'static str, f32)> {
    Some(match function {
        FN_AILERON => ("setup.question.surface_aileron_right_up", 1.0),
        FN_ELEVATOR => ("setup.question.surface_elevator_up", 1.0),
        FN_RUDDER => ("setup.question.surface_rudder_right", 1.0),
        FN_ELEVON_LEFT => ("setup.question.surface_elevon_left_up", 1.0),
        FN_ELEVON_RIGHT => ("setup.question.surface_elevon_right_up", 1.0),
        FN_VTAIL_LEFT => ("setup.question.surface_vtail_left_up", 1.0),
        FN_VTAIL_RIGHT => ("setup.question.surface_vtail_right_up", 1.0),
        _ => return None,
    })
}

/// Airspeed limit names, newest first (4.5+ renamed them).
pub const AIRSPEED_MIN: &[&str] = &["AIRSPEED_MIN", "ARSPD_FBW_MIN"];
pub const AIRSPEED_MAX: &[&str] = &["AIRSPEED_MAX", "ARSPD_FBW_MAX"];
/// Cruise airspeed: `AIRSPEED_CRUISE` (m/s) or the older `TRIM_ARSPD_CM` (cm/s).
pub const AIRSPEED_CRUISE: &[&str] = &["AIRSPEED_CRUISE", "TRIM_ARSPD_CM"];
