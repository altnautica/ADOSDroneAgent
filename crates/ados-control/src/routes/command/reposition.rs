//! `reposition`: fly a flight controller in GUIDED to a picked point.
//!
//! `args = [lat, lon, alt_m]`, degrees and metres above home. The frame is a
//! `COMMAND_INT` `MAV_CMD_DO_REPOSITION` (192): `x`/`y` carry the position as
//! degrees·1e7, so the point keeps its full precision (a `COMMAND_LONG` float
//! param holds a latitude to about a metre only), in the home-relative frame,
//! with `param1 = -1` (the vehicle's default speed), `param2 = 1`
//! (`MAV_DO_REPOSITION_FLAGS_CHANGE_MODE`: the FC switches to its guided mode
//! if needed) and `param4 = NaN` (keep the current yaw).
//!
//! Every argument is checked before a frame is built: a non-finite or
//! out-of-range latitude or longitude, or a height outside the cockpit spec's
//! launch range (`layout.json` `launch.alt_min_m..=alt_max_m`), is a 400.

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

/// `MAV_DO_REPOSITION_FLAGS_CHANGE_MODE`: enter the guided mode if needed.
const REPOSITION_CHANGE_MODE: f32 = 1.0;
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
    let (min, max) = *ALT_RANGE_M;
    if !(min..=max).contains(&alt) {
        return Err(bad(format!(
            "height {alt} m is outside {min}..{max} m above home"
        )));
    }
    let frame = COMMAND_INT_DATA {
        param1: DEFAULT_SPEED,
        param2: REPOSITION_CHANGE_MODE,
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

/// Address a built `COMMAND_INT` to the FC `(system, component)`.
pub(super) fn addressed_int(
    mut cmd: COMMAND_INT_DATA,
    (target_system, target_component): (u8, u8),
) -> COMMAND_INT_DATA {
    cmd.target_system = target_system;
    cmd.target_component = target_component;
    cmd
}
