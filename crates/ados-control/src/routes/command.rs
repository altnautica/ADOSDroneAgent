//! Command routes: the text-command executor + the command catalog.
//!
//! `POST /api/command` is the GCS's fire-and-forget control path: it names one of
//! a small fixed set of high-level commands (`arm`, `disarm`, `takeoff`, `land`,
//! `rtl`, `mode`), and the route turns that into the corresponding MAVLink frame
//! and writes it to `/run/ados/mavlink.sock`, which the router forwards to the
//! FC. `GET /api/commands` returns the catalog of those names with descriptions.
//!
//! ## Why this is the WORKING command path
//!
//! On the current Rust-hybrid agent the FastAPI `commands.py` route is
//! effectively broken: it reaches for `fc.connection` (a pymavlink connection
//! object), which is always `None` because the native router owns the FC serial
//! link after the MAVLink-router cutover. So the FastAPI route always returns 503
//! "No MAVLink connection". This native route is the working replacement: it
//! builds the MAVLink frame itself and writes it to the socket the router reads,
//! the same socket the Python `MavlinkIPCClient.send` writes to. The parity
//! target is therefore NOT the broken 503 — it is the MAVLink bytes the
//! `commands.py` pymavlink calls WOULD have produced (`arducopter_arm()`,
//! `command_long_send(...)`, `set_mode_apm(...)`).
//!
//! ## What pymavlink actually emits (verified against the installed pymavlink)
//!
//! Every command maps to a `COMMAND_LONG`. The `commands.py` source reads as if
//! `rtl`/`mode` send a `SET_MODE` message, but the pymavlink helpers it calls do
//! NOT: `arducopter_arm`/`arducopter_disarm`/`command_long_send` send
//! `COMMAND_LONG`, and `set_mode_apm` (which `rtl` and `mode` both reach through)
//! also sends a `COMMAND_LONG` with `MAV_CMD_DO_SET_MODE` (176), NOT a `SET_MODE`
//! message. So all six commands are `COMMAND_LONG`:
//!
//! | cmd      | command opcode                  | params (1..7)                         |
//! |----------|---------------------------------|---------------------------------------|
//! | arm      | MAV_CMD_COMPONENT_ARM_DISARM 400| p1=1.0, rest 0                        |
//! | disarm   | MAV_CMD_COMPONENT_ARM_DISARM 400| p1=0.0, rest 0                        |
//! | takeoff  | MAV_CMD_NAV_TAKEOFF 22          | p7=alt (default 10.0), rest 0         |
//! | land     | MAV_CMD_NAV_LAND 21             | all 0                                 |
//! | rtl      | MAV_CMD_DO_SET_MODE 176         | p1=1.0, p2=6.0 (RTL custom_mode)      |
//! | mode N   | MAV_CMD_DO_SET_MODE 176         | p1=1.0, p2=custom_mode                |
//!
//! `rtl` sends `DO_SET_MODE` with `custom_mode=6` (RTL in the COPTER table), the
//! same frame the `mode RTL` path produces, so the `rtl` shortcut actually
//! commands Return-to-Launch. (The `commands.py` route reached RTL through
//! pymavlink's `set_mode_apm`, whose first positional argument is the mode, so it
//! would have sent `custom_mode=1` — ACRO — not 6; but that route is dead post
//! the MAVLink-router cutover (`fc.connection` is always `None`, so it always
//! 503s), so there is no live behaviour to preserve. This greenfield path sends
//! the correct frame.)
//!
//! ## Source + target identity (R3)
//!
//! The frame the route writes to the socket is forwarded to the FC verbatim (the
//! router does not re-stamp the header), so the header identity is load-bearing.
//! - **Source** `system_id=1, component_id=191`: the agent/companion identity the
//!   router itself stamps on its own FC send path (`mavlink.system_id` /
//!   `mavlink.component_id`, defaults 1/191), so a command from this surface looks
//!   identical on the wire to one the router sent.
//! - **Target** `target_system=1, target_component=1`: the state socket carries no
//!   `target_system`, so this surface defaults to 1/1, correct for the
//!   single-vehicle ArduPilot bench. A follow-up adds the FC's real target system
//!   to the router's state-socket extras so a non-default sysid is honoured.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use ados_protocol::mavlink::ardupilotmega::{MavCmd, MavMessage, COMMAND_LONG_DATA};
use ados_protocol::mavlink::{self, MavHeader};

use crate::routes::detail;
use crate::state::AppState;

/// The command catalog: name → human description. Ported verbatim from the
/// FastAPI `SIMPLE_COMMANDS` so `GET /api/commands` is byte-identical. Order is
/// preserved (it is emitted as a JSON object; serde_json keeps insertion order).
const SIMPLE_COMMANDS: &[(&str, &str)] = &[
    ("arm", "Arm the vehicle"),
    ("disarm", "Disarm the vehicle"),
    ("takeoff", "Takeoff to altitude (args: [altitude_m])"),
    ("land", "Land at current position"),
    ("rtl", "Return to launch"),
    ("mode", "Set flight mode (args: [mode_name])"),
];

/// The source identity stamped on every command frame: the agent/companion
/// identity the router uses on its own FC send path (defaults 1/191), so a
/// command from this surface is wire-identical to one the router sent.
const SOURCE_SYSTEM_ID: u8 = 1;
const SOURCE_COMPONENT_ID: u8 = 191;

/// The target identity: single-vehicle ArduPilot defaults (R3). The state socket
/// carries no target system, so this surface targets 1/1.
const TARGET_SYSTEM: u8 = 1;
const TARGET_COMPONENT: u8 = 1;

/// Default takeoff altitude in metres when the request carries no `args[0]`,
/// matching the FastAPI route's `float(req.args[0]) if req.args else 10.0`.
const DEFAULT_TAKEOFF_ALT_M: f32 = 10.0;

/// The custom-mode flag the DO_SET_MODE param1 carries, matching pymavlink's
/// `MAV_MODE_FLAG_CUSTOM_MODE_ENABLED` (1).
const CUSTOM_MODE_ENABLED: f32 = 1.0;

/// RTL's `custom_mode` in the ArduCopter mode table (`COPTER_MODE_NUMBERS`
/// `RTL → 6`). The `rtl` shortcut sends this so it commands Return-to-Launch,
/// identical to the `mode RTL` path.
const RTL_CUSTOM_MODE: f32 = 6.0;

/// ArduCopter flight-mode name → `custom_mode`, the reverse of the router's
/// COPTER mode table. The `mode` command resolves a mode name to its custom mode
/// here. The FastAPI route resolves the name against the live
/// `conn.mode_mapping()` (the connected vehicle's type-specific map); this
/// surface has no live connection object, so it uses the COPTER table (the design
/// target for the command route). An unknown name is a 400, the same as the
/// FastAPI route's `if mode_name not in mode_map` branch.
const COPTER_MODE_NUMBERS: &[(&str, u32)] = &[
    ("STABILIZE", 0),
    ("ACRO", 1),
    ("ALT_HOLD", 2),
    ("AUTO", 3),
    ("GUIDED", 4),
    ("LOITER", 5),
    ("RTL", 6),
    ("CIRCLE", 7),
    ("LAND", 9),
    ("DRIFT", 11),
    ("SPORT", 13),
    ("FLIP", 14),
    ("AUTOTUNE", 15),
    ("POSHOLD", 16),
    ("BRAKE", 17),
    ("THROW", 18),
    ("AVOID_ADSB", 19),
    ("GUIDED_NOGPS", 20),
    ("SMART_RTL", 21),
    ("FLOWHOLD", 22),
    ("FOLLOW", 23),
    ("ZIGZAG", 24),
    ("SYSTEMID", 25),
    ("AUTOROTATE", 26),
    ("AUTO_RTL", 27),
];

/// The `POST /api/command` request body. Mirrors the FastAPI `CommandRequest`: a
/// `cmd` string and an `args` list of numbers or strings (takeoff reads `args[0]`
/// as the altitude, mode reads `args[0]` as the mode name). `args` defaults to an
/// empty list when omitted.
#[derive(Debug, Deserialize)]
pub struct CommandRequest {
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<Value>,
}

/// `POST /api/command` → `{"status":"ok","cmd":<cmd>, ...}`.
///
/// 503 `{"detail": ...}` when the FC is not connected OR the MAVLink socket send
/// fails (the command never silently drops). 400 `{"detail": ...}` on an unknown
/// command, a `mode` with no name, or an unknown mode name. Otherwise the frame
/// is built and written to the socket and the route returns the command's `ok`
/// shape.
pub async fn execute_command(
    State(state): State<AppState>,
    Json(req): Json<CommandRequest>,
) -> Response {
    // FC-connected gate, read from the live state snapshot (the same field the
    // status route reads). The FastAPI route gates on `fc.connected`; here the
    // snapshot's `fc_connected` is the equivalent. No snapshot / false → 503.
    if !state.fc_connected() {
        return detail(StatusCode::SERVICE_UNAVAILABLE, "FC not connected");
    }

    let cmd = req.cmd.to_lowercase();

    // Build the MAVLink message + the success body for the named command. An
    // unknown command (or a bad mode arg) returns a 4xx here before any send.
    let (msg, body) = match build_command(&cmd, &req.args) {
        Ok(built) => built,
        Err(err) => return err.into_response(),
    };

    // Serialize the v2 frame with the source identity, then hand the raw bytes to
    // the MAVLink client, which length-prefixes them onto the socket.
    let header = MavHeader {
        system_id: SOURCE_SYSTEM_ID,
        component_id: SOURCE_COMPONENT_ID,
        // The router stamps its own sequence on its frames; for a client-written
        // command the sequence is not semantically load-bearing (ArduPilot does
        // not require a specific value on COMMAND_LONG), so 0 is used. Mirrors the
        // fire-and-forget posture the FastAPI route takes.
        sequence: 0,
    };
    let frame = match mavlink::serialize_v2(header, &msg) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::error!(error = %e, cmd = %cmd, "command frame serialize failed");
            return detail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to encode command",
            );
        }
    };

    if let Err(e) = state.mavlink.send(&frame).await {
        // An absent MAVLink socket or a broken write means no live FC link from
        // this surface's view → 503, matching the FastAPI route's no-connection
        // 503. The command is never silently dropped.
        tracing::warn!(error = %e, cmd = %cmd, "command send to the mavlink socket failed");
        return detail(StatusCode::SERVICE_UNAVAILABLE, "No MAVLink connection");
    }

    (StatusCode::OK, Json(body)).into_response()
}

/// A 4xx the command builder raises before any send: an unknown command, a `mode`
/// with no name, or an unknown mode name. Carries the FastAPI status + detail so
/// it renders as the `{"detail"}` shape.
#[derive(Debug)]
struct CommandError {
    status: StatusCode,
    detail: String,
}

impl IntoResponse for CommandError {
    fn into_response(self) -> Response {
        detail(self.status, self.detail)
    }
}

/// Build the MAVLink message and the success body for a named command. Returns a
/// [`CommandError`] (a 400) for an unknown command or a bad `mode` argument.
///
/// Every command is a `COMMAND_LONG`; the per-command opcode + params are the
/// bytes the FastAPI route's pymavlink calls would have produced (see the module
/// table). The success body mirrors each FastAPI return: `arm`/`disarm`/`land`
/// emit `{"status":"ok","cmd":...}`, `takeoff` adds `"altitude"`, `mode` adds
/// `"mode"`.
fn build_command(cmd: &str, args: &[Value]) -> Result<(MavMessage, Value), CommandError> {
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
            let alt = takeoff_altitude(args);
            Ok((
                command_long(
                    MavCmd::MAV_CMD_NAV_TAKEOFF,
                    [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, alt],
                ),
                json!({"status": "ok", "cmd": "takeoff", "altitude": alt}),
            ))
        }
        "land" => Ok((
            command_long(MavCmd::MAV_CMD_NAV_LAND, [0.0; 7]),
            json!({"status": "ok", "cmd": "land"}),
        )),
        "rtl" => Ok((
            // DO_SET_MODE with p1=CUSTOM_MODE_ENABLED and p2=RTL custom_mode (6),
            // the same frame the `mode RTL` path sends, so the `rtl` shortcut
            // commands Return-to-Launch.
            command_long(
                MavCmd::MAV_CMD_DO_SET_MODE,
                [
                    CUSTOM_MODE_ENABLED,
                    RTL_CUSTOM_MODE,
                    0.0,
                    0.0,
                    0.0,
                    0.0,
                    0.0,
                ],
            ),
            json!({"status": "ok", "cmd": "rtl"}),
        )),
        "mode" => {
            // `mode` needs a name; FastAPI: 400 "Mode name required" when args empty.
            let name = match args.first().and_then(arg_as_str) {
                Some(name) => name.to_uppercase(),
                None => {
                    return Err(CommandError {
                        status: StatusCode::BAD_REQUEST,
                        detail: "Mode name required".to_string(),
                    })
                }
            };
            let custom_mode = match copter_mode_number(&name) {
                Some(n) => n,
                None => {
                    return Err(CommandError {
                        status: StatusCode::BAD_REQUEST,
                        detail: format!("Unknown mode: {name}"),
                    })
                }
            };
            Ok((
                // set_mode(mode_map[name]) → set_mode_apm(custom_mode) →
                // DO_SET_MODE p1=CUSTOM_MODE_ENABLED p2=custom_mode.
                command_long(
                    MavCmd::MAV_CMD_DO_SET_MODE,
                    [
                        CUSTOM_MODE_ENABLED,
                        custom_mode as f32,
                        0.0,
                        0.0,
                        0.0,
                        0.0,
                        0.0,
                    ],
                ),
                json!({"status": "ok", "cmd": "mode", "mode": name}),
            ))
        }
        other => Err(CommandError {
            status: StatusCode::BAD_REQUEST,
            detail: format!("Unknown command: {other}"),
        }),
    }
}

/// Build a `COMMAND_LONG` to the default single-vehicle target with the given
/// opcode and the seven params (confirmation is always 0).
fn command_long(command: MavCmd, params: [f32; 7]) -> MavMessage {
    MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
        target_system: TARGET_SYSTEM,
        target_component: TARGET_COMPONENT,
        command,
        confirmation: 0,
        param1: params[0],
        param2: params[1],
        param3: params[2],
        param4: params[3],
        param5: params[4],
        param6: params[5],
        param7: params[6],
    })
}

/// The takeoff altitude from `args[0]`, defaulting to [`DEFAULT_TAKEOFF_ALT_M`]
/// when absent. Mirrors `float(req.args[0]) if req.args else 10.0`: a numeric
/// `args[0]` is used as-is; a non-numeric or absent first arg falls back to the
/// default (a stringly-typed numeric arg is parsed, matching Python's `float()`).
fn takeoff_altitude(args: &[Value]) -> f32 {
    match args.first() {
        Some(Value::Number(n)) => n
            .as_f64()
            .map(|v| v as f32)
            .unwrap_or(DEFAULT_TAKEOFF_ALT_M),
        Some(Value::String(s)) => s.trim().parse::<f32>().unwrap_or(DEFAULT_TAKEOFF_ALT_M),
        _ => DEFAULT_TAKEOFF_ALT_M,
    }
}

/// Read an arg as a string: a JSON string verbatim, or a number stringified
/// (mirroring `str(req.args[0])`), so a mode name passed as a number still
/// resolves. Returns `None` for a null / array / object arg.
fn arg_as_str(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Resolve an ArduCopter mode name to its `custom_mode`, the reverse of the
/// router's COPTER table. `None` for an unknown name (a 400 at the call site).
fn copter_mode_number(name: &str) -> Option<u32> {
    COPTER_MODE_NUMBERS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, num)| *num)
}

/// `GET /api/commands` → `{"commands": {name: description, ...}}`. The catalog,
/// byte-identical to the FastAPI `SIMPLE_COMMANDS`.
pub async fn list_commands() -> Json<Value> {
    let mut commands = serde_json::Map::new();
    for (name, desc) in SIMPLE_COMMANDS {
        commands.insert((*name).to_string(), json!(desc));
    }
    Json(json!({ "commands": commands }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode a serialized v2 frame back into its message for the parity asserts.
    fn round_trip(msg: &MavMessage) -> MavMessage {
        let header = MavHeader {
            system_id: SOURCE_SYSTEM_ID,
            component_id: SOURCE_COMPONENT_ID,
            sequence: 0,
        };
        let frame = mavlink::serialize_v2(header, msg).unwrap();
        let (_h, decoded) = mavlink::parse_v2(&frame).unwrap();
        decoded
    }

    fn long(msg: &MavMessage) -> COMMAND_LONG_DATA {
        match round_trip(msg) {
            MavMessage::COMMAND_LONG(d) => d,
            other => panic!("expected COMMAND_LONG, got {other:?}"),
        }
    }

    #[test]
    fn arm_is_component_arm_disarm_param1_one() {
        let (msg, body) = build_command("arm", &[]).unwrap();
        let d = long(&msg);
        assert_eq!(d.command, MavCmd::MAV_CMD_COMPONENT_ARM_DISARM);
        assert_eq!(d.param1, 1.0);
        assert_eq!(d.target_system, 1);
        assert_eq!(d.target_component, 1);
        assert_eq!(body["cmd"], json!("arm"));
        assert_eq!(body["status"], json!("ok"));
    }

    #[test]
    fn disarm_is_component_arm_disarm_param1_zero() {
        let (msg, _b) = build_command("disarm", &[]).unwrap();
        let d = long(&msg);
        assert_eq!(d.command, MavCmd::MAV_CMD_COMPONENT_ARM_DISARM);
        assert_eq!(d.param1, 0.0);
    }

    #[test]
    fn takeoff_default_alt_is_ten_in_param7() {
        let (msg, body) = build_command("takeoff", &[]).unwrap();
        let d = long(&msg);
        assert_eq!(d.command, MavCmd::MAV_CMD_NAV_TAKEOFF);
        assert_eq!(d.param7, 10.0);
        assert_eq!(body["altitude"], json!(10.0));
    }

    #[test]
    fn takeoff_reads_the_altitude_arg_numeric_and_string() {
        let (msg, body) = build_command("takeoff", &[json!(25.0)]).unwrap();
        assert_eq!(long(&msg).param7, 25.0);
        assert_eq!(body["altitude"], json!(25.0));
        // A stringly-typed numeric arg parses, matching Python float().
        let (msg2, _b) = build_command("takeoff", &[json!("30")]).unwrap();
        assert_eq!(long(&msg2).param7, 30.0);
    }

    #[test]
    fn land_is_nav_land_all_zero() {
        let (msg, _b) = build_command("land", &[]).unwrap();
        let d = long(&msg);
        assert_eq!(d.command, MavCmd::MAV_CMD_NAV_LAND);
        for p in [
            d.param1, d.param2, d.param3, d.param4, d.param5, d.param6, d.param7,
        ] {
            assert_eq!(p, 0.0);
        }
    }

    #[test]
    fn rtl_is_do_set_mode_param2_six() {
        // The `rtl` shortcut commands Return-to-Launch: DO_SET_MODE custom_mode=6,
        // the same frame the `mode RTL` path sends.
        let (msg, _b) = build_command("rtl", &[]).unwrap();
        let d = long(&msg);
        assert_eq!(d.command, MavCmd::MAV_CMD_DO_SET_MODE);
        assert_eq!(d.param1, 1.0);
        assert_eq!(d.param2, 6.0, "RTL custom_mode is 6 in the COPTER table");
    }

    #[test]
    fn mode_rtl_is_do_set_mode_param2_six() {
        let (msg, body) = build_command("mode", &[json!("rtl")]).unwrap();
        let d = long(&msg);
        assert_eq!(d.command, MavCmd::MAV_CMD_DO_SET_MODE);
        assert_eq!(d.param1, 1.0);
        assert_eq!(d.param2, 6.0, "RTL custom_mode is 6 in the COPTER table");
        // The body echoes the upper-cased mode name.
        assert_eq!(body["mode"], json!("RTL"));
        assert_eq!(body["cmd"], json!("mode"));
    }

    #[test]
    fn mode_guided_resolves_to_four() {
        let (msg, _b) = build_command("mode", &[json!("GUIDED")]).unwrap();
        assert_eq!(long(&msg).param2, 4.0);
    }

    #[test]
    fn mode_with_no_name_is_a_400() {
        let err = build_command("mode", &[]).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.detail, "Mode name required");
    }

    #[test]
    fn unknown_mode_name_is_a_400() {
        let err = build_command("mode", &[json!("NOPE")]).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.detail, "Unknown mode: NOPE");
    }

    #[test]
    fn unknown_command_is_a_400() {
        let err = build_command("fly-to-the-moon", &[]).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.detail, "Unknown command: fly-to-the-moon");
    }

    #[test]
    fn catalog_has_the_six_simple_commands() {
        assert_eq!(SIMPLE_COMMANDS.len(), 6);
        let names: Vec<_> = SIMPLE_COMMANDS.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, ["arm", "disarm", "takeoff", "land", "rtl", "mode"]);
    }
}
