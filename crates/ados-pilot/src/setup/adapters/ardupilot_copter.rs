//! ArduCopter: step lists, modes, motor layouts and the initial-tune formulas.

use super::Mode;

pub const GROUND: &[&str] = &[
    "detect",
    "frame",
    "accel",
    "level",
    "compass",
    "radio",
    "rc_ados_switch",
    "outputs",
    "battery",
    "initial_params",
    "notch_static",
    "ados_profile",
    "ados_intro",
];

pub const AIR: &[&str] = &[
    "preflight",
    "first_hover",
    "notch_air",
    "autotune",
    "verify",
];

pub fn mode(m: Mode) -> Option<u32> {
    Some(match m {
        Mode::Guided => 4,
        Mode::Land => 9,
        Mode::Rtl => 6,
        Mode::Loiter => 5,
        Mode::Autotune => 15,
        _ => return None,
    })
}

/// One motor of a frame layout: ArduPilot motor number (output function
/// `Motor<n>` = 32 + n), angle from the nose (deg, clockwise positive),
/// spin (`true` = clockwise seen from above) and test-sequence position
/// (1 = A, the front-right motor, then clockwise).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MotorDef {
    pub num: u8,
    pub angle: i16,
    pub cw: bool,
    pub seq: u8,
}

const fn m(num: u8, angle: i16, cw: bool, seq: u8) -> MotorDef {
    MotorDef {
        num,
        angle,
        cw,
        seq,
    }
}

// Layouts ported from ArduPilot `libraries/AP_Motors/AP_MotorsMatrix.cpp`
// (motor order = `add_motor` order; the third field is the testing order).
const QUAD_PLUS: &[MotorDef] = &[
    m(1, 90, false, 2),
    m(2, -90, false, 4),
    m(3, 0, true, 1),
    m(4, 180, true, 3),
];
const QUAD_X: &[MotorDef] = &[
    m(1, 45, false, 1),
    m(2, -135, false, 3),
    m(3, -45, true, 4),
    m(4, 135, true, 2),
];
const QUAD_H: &[MotorDef] = &[
    m(1, 45, true, 1),
    m(2, -135, true, 3),
    m(3, -45, false, 4),
    m(4, 135, false, 2),
];
const HEXA_PLUS: &[MotorDef] = &[
    m(1, 0, true, 1),
    m(2, 180, false, 4),
    m(3, -120, true, 5),
    m(4, 60, false, 2),
    m(5, -60, false, 6),
    m(6, 120, true, 3),
];
const HEXA_X: &[MotorDef] = &[
    m(1, 90, true, 2),
    m(2, -90, false, 5),
    m(3, -30, true, 6),
    m(4, 150, false, 3),
    m(5, 30, false, 1),
    m(6, -150, true, 4),
];
const OCTA_PLUS: &[MotorDef] = &[
    m(1, 0, true, 1),
    m(2, 180, true, 5),
    m(3, 45, false, 2),
    m(4, 135, false, 4),
    m(5, -45, false, 8),
    m(6, -135, false, 6),
    m(7, -90, true, 7),
    m(8, 90, true, 3),
];
const OCTA_X: &[MotorDef] = &[
    m(1, 22, true, 1),
    m(2, -158, true, 5),
    m(3, 68, false, 2),
    m(4, 158, false, 4),
    m(5, -22, false, 8),
    m(6, -112, false, 6),
    m(7, -68, true, 7),
    m(8, 112, true, 3),
];

/// Image key and motor layout for `FRAME_CLASS` / `FRAME_TYPE`.
pub fn layout(frame_class: u8, frame_type: u8) -> Option<(&'static str, &'static [MotorDef])> {
    Some(match (frame_class, frame_type) {
        (1, 0) => ("motor_order_quad_plus", QUAD_PLUS),
        (1, 1) => ("motor_order_quad_x", QUAD_X),
        (1, 3) => ("motor_order_quad_h", QUAD_H),
        (2, 0) => ("motor_order_hexa_plus", HEXA_PLUS),
        (2, 1) => ("motor_order_hexa_x", HEXA_X),
        (3, 0) => ("motor_order_octa_plus", OCTA_PLUS),
        (3, 1) => ("motor_order_octa_x", OCTA_X),
        _ => return None,
    })
}

/// Battery chemistry cell limits `(min, max)` V.
pub fn cell_limits(chemistry: &str) -> (f32, f32) {
    match chemistry {
        "lihv" => (3.3, 4.35),
        "liion" => (2.8, 4.1),
        _ => (3.3, 4.2),
    }
}

/// Mission Planner's `RoundTo`: negative precision rounds to tens/hundreds.
fn round_to(value: f64, precision: i32) -> f64 {
    if precision >= 0 {
        let k = 10f64.powi(precision);
        return (value * k).round() / k;
    }
    let p = 10f64.powi(-precision);
    let v = value + 5.0 * p / 10.0;
    (v - v % p).round()
}

/// The initial tune parameters, ported from Mission Planner's
/// `ConfigInitialParams.cs` (`calc_values` and the 4.x parameter names).
/// `atc` / `mot` are the prefixes (`ATC`/`MOT`, or `Q_A`/`Q_M` on a VTOL).
/// `new_accel_names` selects `<atc>_ACCEL_*_MAX` (cdeg/s²) when the FC has
/// them, else `<atc>_ACC_*_MAX` (deg/s²).
pub fn initial_params(
    prop_in: f64,
    cells: f64,
    chemistry: &str,
    atc: &str,
    mot: &str,
    new_accel_names: bool,
) -> Vec<(String, f32)> {
    let (cell_min, cell_max) = cell_limits(chemistry);
    let (cell_min, cell_max) = (cell_min as f64, cell_max as f64);
    let accel_y_max = 8000f64.max(round_to(-900.0 * prop_in + 36000.0, -2));
    let acro_yaw_p = 0.5 * accel_y_max / 4500.0;
    let accel_p_max = 10000f64.max(round_to(
        -2.613267 * prop_in.powi(3) + 343.39216 * prop_in.powi(2) - 15083.7121 * prop_in + 235771.0,
        -2,
    ));
    let gyro_filter = 20f64.max((289.22 * prop_in.powf(-0.838)).round());
    let flt = 10f64.max(gyro_filter / 2.0);
    let thst_expo = ((0.15686 * prop_in.ln() + 0.23693) * 100.0).round() / 100.0;
    let thst_expo = thst_expo.min(0.80);
    let mut v: Vec<(String, f64)> = vec![("ACRO_YAW_P".into(), acro_yaw_p)];
    if new_accel_names {
        v.push((format!("{atc}_ACCEL_P_MAX"), accel_p_max));
        v.push((format!("{atc}_ACCEL_R_MAX"), accel_p_max));
        v.push((format!("{atc}_ACCEL_Y_MAX"), accel_y_max));
    } else {
        v.push((format!("{atc}_ACC_P_MAX"), accel_p_max / 100.0));
        v.push((format!("{atc}_ACC_R_MAX"), accel_p_max / 100.0));
        v.push((format!("{atc}_ACC_Y_MAX"), accel_y_max / 100.0));
    }
    for (k, val) in [
        ("RAT_PIT_FLTD", flt),
        ("RAT_PIT_FLTE", 0.0),
        ("RAT_PIT_FLTT", flt),
        ("RAT_RLL_FLTD", flt),
        ("RAT_RLL_FLTE", 0.0),
        ("RAT_RLL_FLTT", flt),
        ("RAT_YAW_FLTD", 0.0),
        ("RAT_YAW_FLTE", 2.0),
        ("RAT_YAW_FLTT", flt),
        ("THR_MIX_MAN", 0.1),
    ] {
        v.push((format!("{atc}_{k}"), val));
    }
    v.push(("INS_ACCEL_FILTER".into(), 10.0));
    v.push(("INS_GYRO_FILTER".into(), gyro_filter));
    v.push((format!("{mot}_THST_EXPO"), thst_expo));
    v.push((format!("{mot}_THST_HOVER"), 0.2));
    v.push((
        "BATT_ARM_VOLT".into(),
        (cells - 1.0) * 0.1 + (cell_min + 0.3) * cells,
    ));
    v.push(("BATT_CRT_VOLT".into(), (cell_min + 0.2) * cells));
    v.push(("BATT_LOW_VOLT".into(), (cell_min + 0.3) * cells));
    v.push((format!("{mot}_BAT_VOLT_MAX"), cell_max * cells));
    v.push((format!("{mot}_BAT_VOLT_MIN"), cell_min * cells));
    // Learn the hover throttle in flight (feeds the notch and the autotune).
    v.push((format!("{mot}_HOVER_LEARN"), 2.0));
    v.into_iter().map(|(k, x)| (k, x as f32)).collect()
}

/// Hover rotor frequency (Hz): KV · cells · 3.7 V · hover throttle / 60.
pub fn rotor_hz(kv: f32, cells: f32, hover_throttle: f32) -> f32 {
    kv * cells * 3.7 * hover_throttle / 60.0
}
