//! Building the `COMMAND_LONG` for a named command and its flight-mode encoding.

use super::*;

/// Build the `COMMAND_LONG` and the success body for a named command, resolving
/// the flight-mode encoding for the FC's autopilot family and vehicle type.
/// Returns a [`CommandError`] (a 4xx) for an unknown command, a bad `mode`
/// argument, or a mode change the vehicle type cannot be encoded for.
pub(super) fn build_command(
    cmd: &str,
    args: &[Value],
    fc: FcIdentity,
) -> Result<(COMMAND_LONG_DATA, Value), CommandError> {
    match cmd {
        "arm" => Ok((
            command_long(
                MavCmd::MAV_CMD_COMPONENT_ARM_DISARM,
                [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            ),
            json!({"status": "ok", "cmd": "arm"}),
        )),
        "disarm" => Ok((
            command_long(
                MavCmd::MAV_CMD_COMPONENT_ARM_DISARM,
                [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            ),
            json!({"status": "ok", "cmd": "disarm"}),
        )),
        "takeoff" => {
            let alt = takeoff_altitude(args)?;
            Ok((
                command_long(
                    MavCmd::MAV_CMD_NAV_TAKEOFF,
                    [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, alt],
                ),
                json!({"status": "ok", "cmd": "takeoff", "altitude": alt}),
            ))
        }
        "land" => Ok((land_command(fc), json!({"status": "ok", "cmd": "land"}))),
        "rtl" => {
            // The `rtl` shortcut commands Return-to-Launch, the same frame the
            // `mode RTL` path produces for this vehicle.
            let params = set_mode_params_for_name("RTL", fc)?;
            Ok((
                command_long(MavCmd::MAV_CMD_DO_SET_MODE, params),
                json!({"status": "ok", "cmd": "rtl"}),
            ))
        }
        "killswitch" => Ok((
            // Emergency motor cut: disarm (param1=0) with the force magic
            // (param2=21196) so the FC disarms in flight instead of refusing.
            command_long(
                MavCmd::MAV_CMD_COMPONENT_ARM_DISARM,
                [0.0, FORCE_DISARM_MAGIC, 0.0, 0.0, 0.0, 0.0, 0.0],
            ),
            json!({"status": "ok", "cmd": "killSwitch"}),
        )),
        "pausemission" => Ok((
            command_long(
                MavCmd::MAV_CMD_DO_PAUSE_CONTINUE,
                [PAUSE_CONTINUE_PAUSE, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            ),
            json!({"status": "ok", "cmd": "pauseMission"}),
        )),
        "resumemission" => Ok((
            command_long(
                MavCmd::MAV_CMD_DO_PAUSE_CONTINUE,
                [PAUSE_CONTINUE_CONTINUE, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            ),
            json!({"status": "ok", "cmd": "resumeMission"}),
        )),
        "mode" => {
            // `mode` needs a name; 400 "Mode name required" when args empty.
            let name = match args.first().and_then(arg_as_str) {
                Some(name) => name.to_uppercase(),
                None => {
                    return Err(CommandError {
                        status: StatusCode::BAD_REQUEST,
                        detail: "Mode name required".to_string(),
                    })
                }
            };
            let params = set_mode_params_for_name(&name, fc)?;
            Ok((
                command_long(MavCmd::MAV_CMD_DO_SET_MODE, params),
                json!({"status": "ok", "cmd": "mode", "mode": name}),
            ))
        }
        other => Err(CommandError {
            status: StatusCode::BAD_REQUEST,
            detail: format!("Unknown command: {other}"),
        }),
    }
}

/// The landing command for the connected vehicle.
///
/// ArduPlane answers `NAV_LAND` with UNSUPPORTED, so a Plane lands by type: a
/// QuadPlane (a VTOL or multirotor MAV_TYPE, or `Q_ENABLE` set) switches to
/// QLAND, a plain fixed-wing starts its landing sequence with `DO_LAND_START`.
/// Copter, PX4 and anything unidentified keep `NAV_LAND`.
pub(super) fn land_command(fc: FcIdentity) -> COMMAND_LONG_DATA {
    let plane = fc.autopilot != MAV_AUTOPILOT_PX4
        && (fc.q_enabled
            || ArduPilotFirmware::resolve(fc.firmware, fc.mav_type)
                == Some(ArduPilotFirmware::Plane));
    if !plane {
        return command_long(MavCmd::MAV_CMD_NAV_LAND, [0.0; 7]);
    }
    let vtol_type = fc.mav_type != 1;
    match ArduPilotFirmware::Plane.custom_mode("QLAND") {
        Some(qland) if fc.q_enabled || vtol_type => {
            command_long(MavCmd::MAV_CMD_DO_SET_MODE, do_set_mode_ardupilot(qland))
        }
        _ => command_long(MavCmd::MAV_CMD_DO_LAND_START, [0.0; 7]),
    }
}

/// The `DO_SET_MODE` params for a named flight mode on the connected FC.
///
/// PX4 takes the de-packed `(main, sub)` in `param2`/`param3`. ArduPilot takes
/// the flat `custom_mode` from the table for the FC's firmware, as its banner
/// named it (a MAV_TYPE only one firmware reports also serves). An unresolved
/// firmware is a 409: a copter-typed heartbeat may be a QuadPlane, where the
/// copter RTL number commands FBWB. A name missing from the resolved table is a
/// 400.
pub(super) fn set_mode_params_for_name(
    name: &str,
    fc: FcIdentity,
) -> Result<[f32; 7], CommandError> {
    let unknown_mode = || CommandError {
        status: StatusCode::BAD_REQUEST,
        detail: format!("Unknown mode: {name}"),
    };
    if fc.autopilot == MAV_AUTOPILOT_PX4 {
        return px4_mode_number(name)
            .map(|(main, sub)| do_set_mode_px4(main, sub))
            .ok_or_else(unknown_mode);
    }
    let Some(firmware) = ArduPilotFirmware::resolve(fc.firmware, fc.mav_type) else {
        let detail = if ArduPilotFirmware::from_mav_type(fc.mav_type).is_some() {
            format!(
                "The flight controller's firmware is not identified yet (vehicle type {} \
                 is reported by more than one firmware); refusing to set a mode",
                fc.mav_type
            )
        } else {
            format!(
                "No flight-mode table for vehicle type {}; refusing to set a mode",
                fc.mav_type
            )
        };
        return Err(CommandError {
            status: StatusCode::CONFLICT,
            detail,
        });
    };
    firmware
        .custom_mode(name)
        .map(do_set_mode_ardupilot)
        .ok_or_else(unknown_mode)
}

/// `DO_SET_MODE` params for ArduPilot: `param1 = custom-mode-enabled`, `param2 =
/// custom_mode`, the rest 0.
pub(super) fn do_set_mode_ardupilot(custom_mode: u32) -> [f32; 7] {
    [
        CUSTOM_MODE_ENABLED,
        custom_mode as f32,
        0.0,
        0.0,
        0.0,
        0.0,
        0.0,
    ]
}

/// `DO_SET_MODE` params for PX4: `param1 = custom-mode-enabled`, `param2 =
/// main_mode`, `param3 = sub_mode` (de-packed, NOT the 32-bit union value), the
/// rest 0.
pub(super) fn do_set_mode_px4(main_mode: u8, sub_mode: u8) -> [f32; 7] {
    [
        CUSTOM_MODE_ENABLED,
        main_mode as f32,
        sub_mode as f32,
        0.0,
        0.0,
        0.0,
        0.0,
    ]
}

/// Build an unaddressed `COMMAND_LONG` with the given opcode and the seven
/// params (confirmation is 0; the send path bumps it on a resend). The caller
/// addresses it with [`addressed`] once the request's FC target is resolved.
pub(super) fn command_long(command: MavCmd, params: [f32; 7]) -> COMMAND_LONG_DATA {
    COMMAND_LONG_DATA {
        target_system: 0,
        target_component: 0,
        command,
        confirmation: 0,
        param1: params[0],
        param2: params[1],
        param3: params[2],
        param4: params[3],
        param5: params[4],
        param6: params[5],
        param7: params[6],
    }
}

/// Address a built `COMMAND_LONG` to the FC `(system, component)`.
pub(super) fn addressed(
    mut long: COMMAND_LONG_DATA,
    (target_system, target_component): (u8, u8),
) -> COMMAND_LONG_DATA {
    long.target_system = target_system;
    long.target_component = target_component;
    long
}

/// The takeoff altitude from `args[0]`, defaulting to [`DEFAULT_TAKEOFF_ALT_M`]
/// only when the request carries no args at all. Every arg that IS present is
/// validated, never coerced: a 400 here happens before any frame is built.
///
/// The value lands in `param7` of `MAV_CMD_NAV_TAKEOFF` and goes on the wire
/// verbatim, so an unchecked arg is a climb target the FC acts on. Rust's
/// `f32: FromStr` accepts `nan` / `inf` / `infinity` case-insensitively, and an
/// out-of-range magnitude (`1e39`, or a JSON number past f32) saturates to
/// `±inf` on the cast — so the coercing version of this function handed the
/// flight controller a non-finite altitude, which ArduPilot and PX4 each resolve
/// in their own undefined way, and a negative one commands a descent from a
/// takeoff request. A non-numeric arg was worse than either: it silently became
/// 10 m, so the aircraft flew to an altitude nobody asked for.
///
/// The finiteness check must run AFTER the `f32` cast, because that is where a
/// legal-but-oversized `f64` becomes an infinity. Same refusal the param write
/// applies to a non-finite value (`params_write::set_param`).
pub(super) fn takeoff_altitude(args: &[Value]) -> Result<f32, CommandError> {
    let Some(arg) = args.first() else {
        return Ok(DEFAULT_TAKEOFF_ALT_M);
    };
    let parsed = match arg {
        Value::Number(n) => n.as_f64().map(|v| v as f32),
        Value::String(s) => s.trim().parse::<f32>().ok(),
        _ => None,
    };
    match parsed {
        Some(alt) if alt.is_finite() && alt > 0.0 && alt <= MAX_TAKEOFF_ALT_M => Ok(alt),
        _ => Err(CommandError {
            status: StatusCode::BAD_REQUEST,
            detail: format!(
                "takeoff altitude must be a finite number greater than 0 and at \
                 most {MAX_TAKEOFF_ALT_M} metres"
            ),
        }),
    }
}

/// Read an arg as a string: a JSON string verbatim, or a number stringified
/// (mirroring `str(req.args[0])`), so a mode name passed as a number still
/// resolves. Returns `None` for a null / array / object arg.
pub(super) fn arg_as_str(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Resolve a PX4 mode name to its `(main_mode, sub_mode)`. `None` for an unknown
/// name (a 400 at the call site).
pub(super) fn px4_mode_number(name: &str) -> Option<(u8, u8)> {
    PX4_MODE_NUMBERS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, main, sub)| (*main, *sub))
}
