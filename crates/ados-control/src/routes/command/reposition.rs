//! `reposition`: fly a flight controller in GUIDED to a picked point.
//!
//! `args = [lat, lon, alt_m]`, degrees and metres above home. The frame is a
//! `COMMAND_INT` `MAV_CMD_DO_REPOSITION` (192): `x`/`y` carry the position as
//! degrees·1e7, so the point keeps its full precision (a `COMMAND_LONG` float
//! param holds a latitude to about a metre only), in the home-relative frame,
//! with `param1 = -1` (the vehicle's default speed), `param2 = 0` (no
//! `MAV_DO_REPOSITION_FLAGS_CHANGE_MODE`: the FC must already fly its guided
//! mode, so a reposition never takes it out of another mode) and
//! `param4 = NaN` (keep the current yaw).
//!
//! Every argument is checked before a frame is built: a non-finite or
//! out-of-range latitude or longitude, the null point (0, 0), or a height
//! outside the cockpit spec's launch range (`layout.json`
//! `launch.alt_min_m..=alt_max_m`), is a 400.
//!
//! A reposition never interrupts a failsafe: while a fresh pilot frame reports
//! `fc_failsafe`, the FC heartbeat reports a critical or emergency state, or
//! the FC flies a return or landing mode, the route answers 409
//! `{"detail", "code": "failsafe"}` before any frame is built. Outside
//! ArduPilot GUIDED (PX4 included) it answers 409 `code: "bad_state"`.

use std::sync::LazyLock;

use super::*;

/// The cockpit spec's launch heights bound a reposition height too.
const LAYOUT_JSON: &str = include_str!("../../../../../web-shared/src/cockpit-spec/layout.json");

static ALT_RANGE_M: LazyLock<(f64, f64)> = LazyLock::new(|| {
    let layout: Value = serde_json::from_str(LAYOUT_JSON).expect("cockpit-spec layout.json parses");
    let bound = |k: &str| {
        layout["launch"][k]
            .as_f64()
            .unwrap_or_else(|| panic!("layout.json launch.{k} is a number"))
    };
    (bound("alt_min_m"), bound("alt_max_m"))
});

/// `param2` 0: no `MAV_DO_REPOSITION_FLAGS_CHANGE_MODE`, the FC keeps its mode.
const REPOSITION_FLAGS: f32 = 0.0;
/// `param1` -1: fly at the vehicle's default speed.
const DEFAULT_SPEED: f32 = -1.0;

fn bad(detail: String) -> CommandError {
    CommandError {
        status: StatusCode::BAD_REQUEST,
        detail,
    }
}

/// One numeric argument (a JSON number or a numeric string), finite.
fn number(args: &[Value], i: usize, name: &str) -> Result<f64, CommandError> {
    let v = match args.get(i) {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    };
    v.filter(|v| v.is_finite()).ok_or_else(|| {
        bad(format!(
            "reposition needs a finite {name} (args: [lat, lon, alt_m])"
        ))
    })
}

/// Build the unaddressed `DO_REPOSITION` and the success body, or a 400.
pub(super) fn reposition_command(
    args: &[Value],
) -> Result<(COMMAND_INT_DATA, Value), CommandError> {
    let lat = number(args, 0, "latitude")?;
    let lon = number(args, 1, "longitude")?;
    let alt = number(args, 2, "height")?;
    if lat.abs() > 90.0 {
        return Err(bad(format!("latitude {lat} is outside -90..90")));
    }
    if lon.abs() > 180.0 {
        return Err(bad(format!("longitude {lon} is outside -180..180")));
    }
    if lat == 0.0 && lon == 0.0 {
        return Err(bad("the point (0, 0) is not a picked position".to_string()));
    }
    let (min, max) = *ALT_RANGE_M;
    if !(min..=max).contains(&alt) {
        return Err(bad(format!(
            "height {alt} m is outside {min}..{max} m above home"
        )));
    }
    let frame = COMMAND_INT_DATA {
        param1: DEFAULT_SPEED,
        param2: REPOSITION_FLAGS,
        param3: 0.0,
        param4: f32::NAN,
        x: (lat * 1e7).round() as i32,
        y: (lon * 1e7).round() as i32,
        z: alt as f32,
        command: MavCmd::MAV_CMD_DO_REPOSITION,
        target_system: 0,
        target_component: 0,
        frame: MavFrame::MAV_FRAME_GLOBAL_RELATIVE_ALT,
        current: 0,
        autocontinue: 0,
    };
    let body = json!({"status": "ok", "cmd": "reposition", "lat": lat, "lon": lon, "alt_m": alt});
    Ok((frame, body))
}

/// A pilot frame older than this is not read (the pilot's state cadence).
const PILOT_FRESH_MS: u64 = 2000;

/// Return and landing modes a reposition must not interrupt: ArduPilot names
/// (QuadPlane `QRTL`/`QLAND` included) and the PX4 names the state reports.
const RECOVERY_MODES: &[&str] = &[
    "RTL",
    "LAND",
    "SMART_RTL",
    "QRTL",
    "QLAND",
    "AUTO.RTL",
    "AUTO.LAND",
    "AUTO.PRECLAND",
];

/// Whether the nav hub holds a fresh pilot frame reporting `fc_failsafe`.
fn pilot_reports_failsafe(state: &AppState) -> bool {
    let status = state.nav.status_body_for(None);
    let Some(pilot) = status.get("pilot") else {
        return false;
    };
    let fresh = pilot
        .get("age_ms")
        .and_then(Value::as_u64)
        .is_some_and(|age| age <= PILOT_FRESH_MS);
    fresh
        && pilot
            .pointer("/data/fc_failsafe")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

/// The FC's current mode name from the state snapshot, trimmed.
fn fc_mode(snapshot: Option<&Value>) -> Option<String> {
    snapshot
        .and_then(|s| s.get("mode"))
        .and_then(Value::as_str)
        .map(|m| m.trim().to_string())
}

/// Whether the current FC mode is a return or landing mode.
fn in_recovery_mode(mode: Option<&str>) -> bool {
    mode.is_some_and(|m| RECOVERY_MODES.iter().any(|r| r.eq_ignore_ascii_case(m)))
}

/// HEARTBEAT `system_status` `MAV_STATE_CRITICAL` (5) and `MAV_STATE_EMERGENCY` (6).
const FAILSAFE_STATES: [i64; 2] = [5, 6];

/// Whether the FC's own heartbeat reports a critical or emergency state.
fn heartbeat_reports_failsafe(snapshot: Option<&Value>) -> bool {
    snapshot
        .and_then(|s| s.get("system_status"))
        .and_then(Value::as_i64)
        .is_some_and(|s| FAILSAFE_STATES.contains(&s))
}

/// `MAV_AUTOPILOT_PX4`.
const AUTOPILOT_PX4: i64 = 12;

/// Whether the FC flies ArduPilot GUIDED, the one mode a reposition applies
/// to. PX4 has no guided mode this route accepts.
fn in_guided(snapshot: Option<&Value>, mode: Option<&str>) -> bool {
    let px4 = snapshot
        .and_then(|s| s.get("autopilot"))
        .and_then(Value::as_i64)
        == Some(AUTOPILOT_PX4);
    !px4 && mode.is_some_and(|m| m.eq_ignore_ascii_case("GUIDED"))
}

/// The 409 a reposition gets, else `None`: `code: "failsafe"` while the FC is
/// in a failsafe (a fresh pilot frame, or a critical or emergency heartbeat)
/// or flies a return or landing mode; `code: "bad_state"` when it does not fly
/// ArduPilot GUIDED.
pub(super) fn reposition_refusal(state: &AppState) -> Option<Response> {
    let snapshot = state.state.snapshot();
    let snapshot = snapshot.as_ref();
    let mode = fc_mode(snapshot);
    let mode = mode.as_deref();
    let (why, code) = if pilot_reports_failsafe(state) || heartbeat_reports_failsafe(snapshot) {
        ("the flight controller is in a failsafe", "failsafe")
    } else if in_recovery_mode(mode) {
        ("the flight controller is returning or landing", "failsafe")
    } else if !in_guided(snapshot, mode) {
        (
            "reposition needs the flight controller in GUIDED",
            "bad_state",
        )
    } else {
        return None;
    };
    Some(
        (
            StatusCode::CONFLICT,
            Json(json!({"detail": why, "code": code})),
        )
            .into_response(),
    )
}

/// Address a built `COMMAND_INT` to the FC `(system, component)`.
pub(super) fn addressed_int(
    mut cmd: COMMAND_INT_DATA,
    (target_system, target_component): (u8, u8),
) -> COMMAND_INT_DATA {
    cmd.target_system = target_system;
    cmd.target_component = target_component;
    cmd
}
