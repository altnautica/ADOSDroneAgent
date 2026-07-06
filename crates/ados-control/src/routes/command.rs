//! Command routes: the text-command executor + the command catalog.
//!
//! `POST /api/command` is the GCS's control path: it names one of a small fixed
//! set of high-level commands (`arm`, `disarm`, `takeoff`, `land`, `rtl`,
//! `mode`), and the route turns that into the corresponding MAVLink frame,
//! writes it to `/run/ados/mavlink.sock` (which the router forwards to the FC),
//! and correlates the flight controller's `COMMAND_ACK` so the response reports
//! whether the FC accepted the command. `GET /api/commands` returns the catalog
//! of those names with descriptions.
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
//! ## What each command maps to
//!
//! Every command maps to a `COMMAND_LONG`. `arm`/`disarm`/`takeoff`/`land` are
//! autopilot-agnostic (the same `MAV_CMD` and params on ArduPilot and PX4).
//! `rtl`/`mode` set a flight mode via `MAV_CMD_DO_SET_MODE` (176), and the mode
//! encoding differs by autopilot family:
//!
//! | cmd      | command opcode                  | params (1..7)                         |
//! |----------|---------------------------------|---------------------------------------|
//! | arm      | MAV_CMD_COMPONENT_ARM_DISARM 400| p1=1.0, rest 0                        |
//! | disarm   | MAV_CMD_COMPONENT_ARM_DISARM 400| p1=0.0, rest 0                        |
//! | takeoff  | MAV_CMD_NAV_TAKEOFF 22          | p7=alt (default 10.0), rest 0         |
//! | land     | MAV_CMD_NAV_LAND 21             | all 0                                 |
//! | rtl      | MAV_CMD_DO_SET_MODE 176         | ArduPilot: p1=1, p2=6 · PX4: p1=1, p2=4, p3=5 |
//! | mode N   | MAV_CMD_DO_SET_MODE 176         | ArduPilot: p1=1, p2=custom_mode · PX4: p1=1, p2=main, p3=sub |
//!
//! ## Flight-mode encoding: ArduPilot vs PX4
//!
//! ArduPilot carries a flat `custom_mode` integer (the copter mode table), so
//! `DO_SET_MODE` sets `param1=1` (custom-mode enabled) and `param2=custom_mode`,
//! `param3=0`.
//!
//! PX4 uses a two-level `(main_mode, sub_mode)` scheme. For the `DO_SET_MODE`
//! **command**, PX4 reads the two levels as SEPARATE small integers:
//! `param2 = main_mode`, `param3 = sub_mode` — it does NOT read a bit-packed
//! value from `param2`. (The `main << 16 | sub << 24` packing is the layout of
//! the 32-bit `custom_mode` FIELD in `HEARTBEAT`/`SET_MODE`, which the state
//! producer DECODES to name the current mode; the command layer takes the two
//! levels de-packed.) So on PX4, RTL is the AUTO main mode with the RTL sub mode
//! (`param2=4`, `param3=5`), not the copter value `6`.
//!
//! The route reads the FC's advertised autopilot from the live state snapshot
//! (`autopilot == 12` is PX4) and picks the encoding accordingly.
//!
//! ## COMMAND_ACK correlation
//!
//! A command is not fire-and-forget: after writing the frame the route reads the
//! FC frame stream back on a dedicated socket connection and correlates the
//! `COMMAND_ACK`. The typed `COMMAND_ACK` in this dialect carries no
//! `target_system`/`target_component` extension fields, so correlation uses the
//! robust cross-stack key: the acknowledged command id AND the ACK's SOURCE
//! system == the vehicle the command addressed. The `MAV_RESULT` is surfaced in
//! the response `ack` block. If the command reports `IN_PROGRESS` the route
//! waits (with a longer bound) for a final result; if no ACK arrives within the
//! bounded retry window (each retry resends with `confirmation++`) the route
//! reports an honest `{"observed": false}` rather than a fabricated success. On
//! a rejection any `STATUSTEXT` seen alongside the ACK is attached as the human
//! reason (ArduPilot reports prearm-style denials only via `STATUSTEXT`).
//!
//! ## Source + target identity
//!
//! The frame the route writes to the socket is forwarded to the FC verbatim (the
//! router does not re-stamp the header), so the header identity is load-bearing.
//! - **Source** `system_id=1, component_id=191`: the agent/companion identity the
//!   router itself stamps on its own FC send path (`mavlink.system_id` /
//!   `mavlink.component_id`, defaults 1/191), so a command from this surface looks
//!   identical on the wire to one the router sent.
//! - **Target** `target_system=1, target_component=1`: the state socket carries no
//!   `target_system`, so this surface defaults to 1/1, correct for the
//!   single-vehicle bench. The ACK's source system is correlated against this
//!   target so an ACK from the addressed vehicle is the one that resolves.

use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use ados_protocol::mavlink::ardupilotmega::{MavCmd, MavMessage, COMMAND_LONG_DATA};
use ados_protocol::mavlink::{self, MavHeader};

use crate::ipc::{FrameRead, MavlinkIpcClient};
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

/// The target identity: single-vehicle defaults. The state socket carries no
/// target system, so this surface targets 1/1.
const TARGET_SYSTEM: u8 = 1;
const TARGET_COMPONENT: u8 = 1;

/// The `HEARTBEAT.autopilot` value for PX4 (`MAV_AUTOPILOT_PX4`). When the FC
/// advertises this, `rtl`/`mode` use the PX4 `(main, sub)` mode encoding; any
/// other value uses the ArduPilot copter mode table.
const AUTOPILOT_PX4: i64 = 12;

/// Default takeoff altitude in metres when the request carries no `args[0]`,
/// matching the FastAPI route's `float(req.args[0]) if req.args else 10.0`.
const DEFAULT_TAKEOFF_ALT_M: f32 = 10.0;

/// The custom-mode flag the DO_SET_MODE param1 carries, matching
/// `MAV_MODE_FLAG_CUSTOM_MODE_ENABLED` (1).
const CUSTOM_MODE_ENABLED: f32 = 1.0;

/// RTL's `custom_mode` in the ArduCopter mode table (`RTL → 6`). The `rtl`
/// shortcut on ArduPilot sends this so it commands Return-to-Launch, identical
/// to the `mode RTL` path.
const COPTER_RTL_CUSTOM_MODE: u32 = 6;

// ── PX4 mode ids (px4_custom_mode.h) ────────────────────────────────────────
// The DO_SET_MODE command takes these de-packed: param2 = main, param3 = sub.

/// PX4 main modes.
const PX4_MAIN_MANUAL: u8 = 1;
const PX4_MAIN_ALTCTL: u8 = 2;
const PX4_MAIN_POSCTL: u8 = 3;
const PX4_MAIN_AUTO: u8 = 4;
const PX4_MAIN_ACRO: u8 = 5;
const PX4_MAIN_OFFBOARD: u8 = 6;
const PX4_MAIN_STABILIZED: u8 = 7;
const PX4_MAIN_RATTITUDE: u8 = 8;

/// PX4 AUTO sub modes (only meaningful when main == AUTO).
const PX4_SUB_AUTO_READY: u8 = 1;
const PX4_SUB_AUTO_TAKEOFF: u8 = 2;
const PX4_SUB_AUTO_LOITER: u8 = 3;
const PX4_SUB_AUTO_MISSION: u8 = 4;
const PX4_SUB_AUTO_RTL: u8 = 5;
const PX4_SUB_AUTO_LAND: u8 = 6;
const PX4_SUB_AUTO_FOLLOW_TARGET: u8 = 8;
const PX4_SUB_AUTO_PRECLAND: u8 = 9;

/// ArduCopter flight-mode name → `custom_mode`, the reverse of the router's
/// COPTER mode table. The `mode` command resolves a mode name to its custom mode
/// here on an ArduPilot FC. An unknown name is a 400.
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

/// PX4 flight-mode name → `(main_mode, sub_mode)` for the `DO_SET_MODE` command
/// path. Mirrors the router's PX4 decode table, with both the short operator
/// names (`RTL`, `LOITER`, `MISSION`, `TAKEOFF`, `LAND`) and the dotted names the
/// state producer reports (`AUTO.RTL`) resolving to the same pair. The `mode`
/// command resolves a mode name here on a PX4 FC.
const PX4_MODE_NUMBERS: &[(&str, u8, u8)] = &[
    ("MANUAL", PX4_MAIN_MANUAL, 0),
    ("ALTCTL", PX4_MAIN_ALTCTL, 0),
    ("ALTITUDE", PX4_MAIN_ALTCTL, 0),
    ("POSCTL", PX4_MAIN_POSCTL, 0),
    ("POSITION", PX4_MAIN_POSCTL, 0),
    ("ACRO", PX4_MAIN_ACRO, 0),
    ("OFFBOARD", PX4_MAIN_OFFBOARD, 0),
    ("STABILIZED", PX4_MAIN_STABILIZED, 0),
    ("RATTITUDE", PX4_MAIN_RATTITUDE, 0),
    ("READY", PX4_MAIN_AUTO, PX4_SUB_AUTO_READY),
    ("AUTO.READY", PX4_MAIN_AUTO, PX4_SUB_AUTO_READY),
    ("TAKEOFF", PX4_MAIN_AUTO, PX4_SUB_AUTO_TAKEOFF),
    ("AUTO.TAKEOFF", PX4_MAIN_AUTO, PX4_SUB_AUTO_TAKEOFF),
    ("LOITER", PX4_MAIN_AUTO, PX4_SUB_AUTO_LOITER),
    ("AUTO.LOITER", PX4_MAIN_AUTO, PX4_SUB_AUTO_LOITER),
    ("MISSION", PX4_MAIN_AUTO, PX4_SUB_AUTO_MISSION),
    ("AUTO.MISSION", PX4_MAIN_AUTO, PX4_SUB_AUTO_MISSION),
    ("AUTO", PX4_MAIN_AUTO, PX4_SUB_AUTO_MISSION),
    ("RTL", PX4_MAIN_AUTO, PX4_SUB_AUTO_RTL),
    ("AUTO.RTL", PX4_MAIN_AUTO, PX4_SUB_AUTO_RTL),
    ("LAND", PX4_MAIN_AUTO, PX4_SUB_AUTO_LAND),
    ("AUTO.LAND", PX4_MAIN_AUTO, PX4_SUB_AUTO_LAND),
    ("FOLLOW_TARGET", PX4_MAIN_AUTO, PX4_SUB_AUTO_FOLLOW_TARGET),
    (
        "AUTO.FOLLOW_TARGET",
        PX4_MAIN_AUTO,
        PX4_SUB_AUTO_FOLLOW_TARGET,
    ),
    ("PRECLAND", PX4_MAIN_AUTO, PX4_SUB_AUTO_PRECLAND),
    ("AUTO.PRECLAND", PX4_MAIN_AUTO, PX4_SUB_AUTO_PRECLAND),
];

// ── MAV_RESULT numeric values (common.xml) ──────────────────────────────────
const MAV_RESULT_ACCEPTED: u8 = 0;
const MAV_RESULT_IN_PROGRESS: u8 = 5;

/// Timing + retry policy for the `COMMAND_ACK` correlation. Injectable so a test
/// can drive short bounds; the route uses [`AckConfig::default`].
#[derive(Debug, Clone, Copy)]
struct AckConfig {
    /// How long to wait for an ACK after each send before resending.
    base_timeout: Duration,
    /// How many resends to attempt (with `confirmation++`) after the first send
    /// times out. `0` sends exactly once.
    retries: u32,
    /// The extended wait after an `IN_PROGRESS` ACK — the command was accepted
    /// and is executing, so the route waits longer for a final result and does
    /// not resend.
    inprogress_timeout: Duration,
}

impl Default for AckConfig {
    fn default() -> Self {
        Self {
            base_timeout: Duration::from_millis(1200),
            retries: 2,
            inprogress_timeout: Duration::from_secs(8),
        }
    }
}

/// The outcome of a correlated command send.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AckOutcome {
    /// A `COMMAND_ACK` matching this command arrived. Carries the `MAV_RESULT`
    /// and, for a non-accepted result, any `STATUSTEXT` seen alongside it (the
    /// human-readable reason ArduPilot reports for a denial).
    Acked {
        result: u8,
        statustext: Option<String>,
    },
    /// The command was written to the FC but no matching ACK arrived within the
    /// bounded retry window. Honest: the command was sent, the ACK was not seen —
    /// never reported as a fabricated success.
    NoAck,
}

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

/// `POST /api/command` → `{"status":"ok","cmd":<cmd>, "ack": {...}}`.
///
/// 503 `{"detail": ...}` when the FC is not connected OR the MAVLink socket
/// cannot be reached (the command never silently drops). 400 `{"detail": ...}`
/// on an unknown command, a `mode` with no name, or an unknown mode name.
/// Otherwise the frame is built, written to the socket, and the FC's
/// `COMMAND_ACK` is correlated into the `ack` block of the response.
pub async fn execute_command(
    State(state): State<AppState>,
    Json(req): Json<CommandRequest>,
) -> Response {
    // FC-connected gate, read from the live state snapshot (the same field the
    // status route reads). No snapshot / false → 503.
    if !state.fc_connected() {
        return detail(StatusCode::SERVICE_UNAVAILABLE, "FC not connected");
    }

    let cmd = req.cmd.to_lowercase();
    let autopilot = state.autopilot();

    // Build the COMMAND_LONG + the success body for the named command. An unknown
    // command (or a bad mode arg) returns a 4xx here before any send.
    let (base_long, mut body) = match build_command(&cmd, &req.args, autopilot) {
        Ok(built) => built,
        Err(err) => return err.into_response(),
    };

    // Send the command and correlate the FC's COMMAND_ACK on a dedicated socket
    // connection. An absent MAVLink socket or a broken write means no live FC
    // link → 503, matching the FastAPI route's no-connection 503.
    match send_awaiting_ack(&state.mavlink, &base_long, &AckConfig::default()).await {
        Ok(outcome) => {
            merge_ack(&mut body, &outcome);
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, cmd = %cmd, "command send/ack exchange failed");
            detail(StatusCode::SERVICE_UNAVAILABLE, "No MAVLink connection")
        }
    }
}

/// A 4xx the command builder raises before any send: an unknown command, a `mode`
/// with no name, or an unknown mode name.
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

/// Build the `COMMAND_LONG` and the success body for a named command, resolving
/// the flight-mode encoding for the FC's `autopilot` family. Returns a
/// [`CommandError`] (a 400) for an unknown command or a bad `mode` argument.
fn build_command(
    cmd: &str,
    args: &[Value],
    autopilot: i64,
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
        "rtl" => {
            // The `rtl` shortcut commands Return-to-Launch, the same frame the
            // `mode RTL` path produces for this autopilot family.
            let params = set_mode_params_for_rtl(autopilot);
            Ok((
                command_long(MavCmd::MAV_CMD_DO_SET_MODE, params),
                json!({"status": "ok", "cmd": "rtl"}),
            ))
        }
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
            let params = match set_mode_params_for_name(&name, autopilot) {
                Some(p) => p,
                None => {
                    return Err(CommandError {
                        status: StatusCode::BAD_REQUEST,
                        detail: format!("Unknown mode: {name}"),
                    })
                }
            };
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

/// The `DO_SET_MODE` params for Return-to-Launch on the given autopilot family.
/// PX4 takes the de-packed `(main, sub)` in `param2`/`param3` (AUTO.RTL =
/// `4`/`5`); ArduPilot takes the flat copter `custom_mode` in `param2` (`6`).
fn set_mode_params_for_rtl(autopilot: i64) -> [f32; 7] {
    if autopilot == AUTOPILOT_PX4 {
        do_set_mode_px4(PX4_MAIN_AUTO, PX4_SUB_AUTO_RTL)
    } else {
        do_set_mode_ardupilot(COPTER_RTL_CUSTOM_MODE)
    }
}

/// The `DO_SET_MODE` params for a named flight mode on the given autopilot
/// family, or `None` when the name is not in that family's mode table (a 400 at
/// the call site).
fn set_mode_params_for_name(name: &str, autopilot: i64) -> Option<[f32; 7]> {
    if autopilot == AUTOPILOT_PX4 {
        px4_mode_number(name).map(|(main, sub)| do_set_mode_px4(main, sub))
    } else {
        copter_mode_number(name).map(do_set_mode_ardupilot)
    }
}

/// `DO_SET_MODE` params for ArduPilot: `param1 = custom-mode-enabled`, `param2 =
/// custom_mode`, the rest 0.
fn do_set_mode_ardupilot(custom_mode: u32) -> [f32; 7] {
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
fn do_set_mode_px4(main_mode: u8, sub_mode: u8) -> [f32; 7] {
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

/// Build a `COMMAND_LONG` to the default single-vehicle target with the given
/// opcode and the seven params (confirmation is 0; the send path bumps it on a
/// resend).
fn command_long(command: MavCmd, params: [f32; 7]) -> COMMAND_LONG_DATA {
    COMMAND_LONG_DATA {
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
    }
}

/// The takeoff altitude from `args[0]`, defaulting to [`DEFAULT_TAKEOFF_ALT_M`]
/// when absent. Mirrors `float(req.args[0]) if req.args else 10.0`: a numeric
/// `args[0]` is used as-is; a stringly-typed numeric arg is parsed; a non-numeric
/// or absent first arg falls back to the default.
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

/// Resolve an ArduCopter mode name to its `custom_mode`. `None` for an unknown
/// name (a 400 at the call site).
fn copter_mode_number(name: &str) -> Option<u32> {
    COPTER_MODE_NUMBERS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, num)| *num)
}

/// Resolve a PX4 mode name to its `(main_mode, sub_mode)`. `None` for an unknown
/// name (a 400 at the call site).
fn px4_mode_number(name: &str) -> Option<(u8, u8)> {
    PX4_MODE_NUMBERS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, main, sub)| (*main, *sub))
}

/// Send `base_long` and correlate the FC's `COMMAND_ACK`.
///
/// Opens a dedicated MAVLink-socket connection, writes the command (with
/// `confirmation = 0`), and reads the broadcast FC frame stream for a matching
/// `COMMAND_ACK`. On a timeout with no ACK it resends with `confirmation++` up to
/// the configured retry budget; on `IN_PROGRESS` it waits (longer) for a final
/// result and stops resending; on a rejection it attaches any `STATUSTEXT` seen
/// in the window. Returns [`AckOutcome::NoAck`] if the retry window closes with no
/// ACK. An absent socket / write failure is an `Err` the route maps to a 503.
async fn send_awaiting_ack(
    client: &MavlinkIpcClient,
    base_long: &COMMAND_LONG_DATA,
    cfg: &AckConfig,
) -> Result<AckOutcome, crate::ipc::mavlink_client::SendError> {
    let expected_cmd = base_long.command;
    let expected_src = base_long.target_system;

    let mut stream = client.open_ack_stream().await?;

    for attempt in 0..=cfg.retries {
        // (Re)send with confirmation = attempt (0 on the first send).
        let mut long = base_long.clone();
        long.confirmation = attempt.min(u8::MAX as u32) as u8;
        let header = MavHeader {
            system_id: SOURCE_SYSTEM_ID,
            component_id: SOURCE_COMPONENT_ID,
            sequence: 0,
        };
        let frame =
            mavlink::serialize_v2(header, &MavMessage::COMMAND_LONG(long)).map_err(|e| {
                crate::ipc::mavlink_client::SendError::Io(std::io::Error::other(e.to_string()))
            })?;
        stream.write_frame(&frame).await?;

        let mut last_statustext: Option<String> = None;
        let mut in_progress = false;
        let mut deadline = Instant::now() + cfg.base_timeout;

        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            match stream.read_frame(deadline - now).await {
                FrameRead::Frame(payload) => {
                    if let Some(result) = match_command_ack(&payload, expected_cmd, expected_src) {
                        if result == MAV_RESULT_IN_PROGRESS {
                            // Accepted and executing: wait longer for a final
                            // result and stop resending.
                            in_progress = true;
                            deadline = Instant::now() + cfg.inprogress_timeout;
                            continue;
                        }
                        let statustext = if result == MAV_RESULT_ACCEPTED {
                            None
                        } else {
                            last_statustext.take()
                        };
                        return Ok(AckOutcome::Acked { result, statustext });
                    } else if let Some(txt) = statustext_of(&payload) {
                        // Buffer the latest STATUSTEXT so a following rejection
                        // ACK can carry the human-readable reason.
                        last_statustext = Some(txt);
                    }
                }
                // Nothing arrived in the window: resend (unless we already saw
                // IN_PROGRESS, handled below), or give up after the budget.
                FrameRead::Timeout => break,
                // The stream closed; no more frames will arrive.
                FrameRead::Eof => return Ok(AckOutcome::NoAck),
            }
        }

        if in_progress {
            // We saw IN_PROGRESS but the extended wait elapsed with no final
            // result. Report the accepted-and-executing state; do not resend.
            return Ok(AckOutcome::Acked {
                result: MAV_RESULT_IN_PROGRESS,
                statustext: None,
            });
        }
        // else: timed out with no ACK → loop to resend with confirmation++,
        // unless the retry budget is exhausted.
    }

    Ok(AckOutcome::NoAck)
}

/// If `frame` is a `COMMAND_ACK` for `expected_cmd` from `expected_src_system`,
/// return its `MAV_RESULT` (as a `u8`). Correlation uses the acknowledged command
/// id AND the ACK's SOURCE system == the vehicle the command addressed — the
/// typed `COMMAND_ACK` in this dialect carries no `target_system` extension
/// field, so this source-system fallback is the correlation key. A frame that is
/// not a parseable `COMMAND_ACK`, is for another command, or is from another
/// system returns `None`.
fn match_command_ack(frame: &[u8], expected_cmd: MavCmd, expected_src_system: u8) -> Option<u8> {
    let (header, msg) = mavlink::parse_any(frame).ok()?;
    match msg {
        MavMessage::COMMAND_ACK(ack)
            if ack.command == expected_cmd && header.system_id == expected_src_system =>
        {
            Some(ack.result as u8)
        }
        _ => None,
    }
}

/// If `frame` is a `STATUSTEXT`, return its text (trailing NULs trimmed). Used to
/// surface the human-readable reason for a command rejection.
fn statustext_of(frame: &[u8]) -> Option<String> {
    let (_header, msg) = mavlink::parse_any(frame).ok()?;
    match msg {
        MavMessage::STATUSTEXT(s) => {
            let end = s.text.iter().position(|&b| b == 0).unwrap_or(s.text.len());
            let text = String::from_utf8_lossy(&s.text[..end]).trim().to_string();
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }
        _ => None,
    }
}

/// The human name for a `MAV_RESULT` value (common.xml).
fn mav_result_name(result: u8) -> &'static str {
    match result {
        0 => "ACCEPTED",
        1 => "TEMPORARILY_REJECTED",
        2 => "DENIED",
        3 => "UNSUPPORTED",
        4 => "FAILED",
        5 => "IN_PROGRESS",
        6 => "CANCELLED",
        _ => "UNKNOWN",
    }
}

/// Merge the ACK outcome into the response body under an `ack` key. Accepted →
/// `{"observed":true,"result":0,"result_name":"ACCEPTED","accepted":true}`; a
/// rejection carries the result + name + any `statustext`; no ACK →
/// `{"observed":false}` (honest, never a fabricated result).
fn merge_ack(body: &mut Value, outcome: &AckOutcome) {
    let ack = match outcome {
        AckOutcome::Acked { result, statustext } => {
            let mut m = json!({
                "observed": true,
                "result": result,
                "result_name": mav_result_name(*result),
                "accepted": *result == MAV_RESULT_ACCEPTED,
            });
            if let Some(txt) = statustext {
                m["statustext"] = json!(txt);
            }
            m
        }
        AckOutcome::NoAck => json!({ "observed": false }),
    };
    if let Value::Object(map) = body {
        map.insert("ack".to_string(), ack);
    }
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
    use ados_protocol::frame::{encode_frame, MAVLINK_MAX_FRAME};
    use ados_protocol::mavlink::ardupilotmega::{
        MavResult, MavSeverity, COMMAND_ACK_DATA, STATUSTEXT_DATA,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    /// A non-PX4 autopilot value (ArduPilot); any value != 12 selects the copter
    /// path. `MAV_AUTOPILOT_ARDUPILOTMEGA` is 3.
    const ARDUPILOT: i64 = 3;

    // ── G6: command building + PX4 mode encoding ────────────────────────────

    #[test]
    fn arm_is_component_arm_disarm_param1_one() {
        let (d, body) = build_command("arm", &[], ARDUPILOT).unwrap();
        assert_eq!(d.command, MavCmd::MAV_CMD_COMPONENT_ARM_DISARM);
        assert_eq!(d.param1, 1.0);
        assert_eq!(d.target_system, 1);
        assert_eq!(d.target_component, 1);
        assert_eq!(body["cmd"], json!("arm"));
        assert_eq!(body["status"], json!("ok"));
    }

    #[test]
    fn disarm_is_component_arm_disarm_param1_zero() {
        let (d, _b) = build_command("disarm", &[], ARDUPILOT).unwrap();
        assert_eq!(d.command, MavCmd::MAV_CMD_COMPONENT_ARM_DISARM);
        assert_eq!(d.param1, 0.0);
    }

    #[test]
    fn takeoff_default_alt_is_ten_in_param7() {
        let (d, body) = build_command("takeoff", &[], ARDUPILOT).unwrap();
        assert_eq!(d.command, MavCmd::MAV_CMD_NAV_TAKEOFF);
        assert_eq!(d.param7, 10.0);
        assert_eq!(body["altitude"], json!(10.0));
    }

    #[test]
    fn takeoff_reads_the_altitude_arg_numeric_and_string() {
        let (d, body) = build_command("takeoff", &[json!(25.0)], ARDUPILOT).unwrap();
        assert_eq!(d.param7, 25.0);
        assert_eq!(body["altitude"], json!(25.0));
        // A stringly-typed numeric arg parses, matching Python float().
        let (d2, _b) = build_command("takeoff", &[json!("30")], ARDUPILOT).unwrap();
        assert_eq!(d2.param7, 30.0);
    }

    #[test]
    fn land_is_nav_land_all_zero() {
        let (d, _b) = build_command("land", &[], ARDUPILOT).unwrap();
        assert_eq!(d.command, MavCmd::MAV_CMD_NAV_LAND);
        for p in [
            d.param1, d.param2, d.param3, d.param4, d.param5, d.param6, d.param7,
        ] {
            assert_eq!(p, 0.0);
        }
    }

    #[test]
    fn ardupilot_rtl_is_do_set_mode_param2_six() {
        // On ArduPilot the `rtl` shortcut sends DO_SET_MODE custom_mode=6.
        let (d, _b) = build_command("rtl", &[], ARDUPILOT).unwrap();
        assert_eq!(d.command, MavCmd::MAV_CMD_DO_SET_MODE);
        assert_eq!(d.param1, 1.0);
        assert_eq!(d.param2, 6.0, "ArduCopter RTL custom_mode is 6");
        assert_eq!(d.param3, 0.0, "ArduPilot uses no sub-mode");
    }

    #[test]
    fn ardupilot_mode_rtl_matches_the_rtl_shortcut() {
        let (d, body) = build_command("mode", &[json!("rtl")], ARDUPILOT).unwrap();
        assert_eq!(d.command, MavCmd::MAV_CMD_DO_SET_MODE);
        assert_eq!(d.param1, 1.0);
        assert_eq!(d.param2, 6.0);
        assert_eq!(body["mode"], json!("RTL"));
        assert_eq!(body["cmd"], json!("mode"));
    }

    #[test]
    fn ardupilot_mode_guided_resolves_to_four() {
        let (d, _b) = build_command("mode", &[json!("GUIDED")], ARDUPILOT).unwrap();
        assert_eq!(d.param2, 4.0);
        assert_eq!(d.param3, 0.0);
    }

    #[test]
    fn px4_rtl_is_do_set_mode_auto_main_rtl_sub() {
        // On PX4 the `rtl` shortcut sends DO_SET_MODE with de-packed main/sub:
        // AUTO (4) in param2, RTL (5) in param3 — NOT the copter value 6.
        let (d, _b) = build_command("rtl", &[], AUTOPILOT_PX4).unwrap();
        assert_eq!(d.command, MavCmd::MAV_CMD_DO_SET_MODE);
        assert_eq!(d.param1, 1.0);
        assert_eq!(d.param2, 4.0, "PX4 RTL main_mode is AUTO=4");
        assert_eq!(d.param3, 5.0, "PX4 RTL sub_mode is 5");
        assert_ne!(d.param2, 6.0, "PX4 RTL is never the ArduCopter value 6");
    }

    #[test]
    fn px4_mode_golden_main_sub_and_packed_values() {
        // Golden (main, sub) for the AUTO modes, plus the packed custom_mode value
        // (main<<16 | sub<<24) each pair decodes to on the HEARTBEAT side — the
        // command uses the de-packed main/sub in param2/param3, and the two must
        // stay consistent with the router's decode table.
        let cases = [
            ("RTL", 4.0_f32, 5.0_f32, 0x0504_0000_u32),
            ("LOITER", 4.0, 3.0, 0x0304_0000),
            ("MISSION", 4.0, 4.0, 0x0404_0000),
            ("TAKEOFF", 4.0, 2.0, 0x0204_0000),
            ("LAND", 4.0, 6.0, 0x0604_0000),
        ];
        for (name, main, sub, packed) in cases {
            let (d, body) = build_command("mode", &[json!(name)], AUTOPILOT_PX4).unwrap();
            assert_eq!(d.param1, 1.0, "{name}: custom-mode-enabled");
            assert_eq!(d.param2, main, "{name}: main_mode in param2");
            assert_eq!(d.param3, sub, "{name}: sub_mode in param3");
            assert_eq!(body["mode"], json!(name));
            // The de-packed main/sub reconstruct the router's packed custom_mode.
            let reconstructed = ((main as u32) << 16) | ((sub as u32) << 24);
            assert_eq!(reconstructed, packed, "{name}: packed custom_mode matches");
        }
    }

    #[test]
    fn px4_single_level_modes_have_zero_sub() {
        // POSCTL / OFFBOARD / MANUAL etc. carry no sub-mode (param3 = 0).
        for (name, main) in [
            ("POSCTL", PX4_MAIN_POSCTL),
            ("POSITION", PX4_MAIN_POSCTL),
            ("OFFBOARD", PX4_MAIN_OFFBOARD),
            ("MANUAL", PX4_MAIN_MANUAL),
            ("ALTITUDE", PX4_MAIN_ALTCTL),
            ("ACRO", PX4_MAIN_ACRO),
            ("STABILIZED", PX4_MAIN_STABILIZED),
        ] {
            let (d, _b) = build_command("mode", &[json!(name)], AUTOPILOT_PX4).unwrap();
            assert_eq!(d.param2, main as f32, "{name}: main_mode");
            assert_eq!(d.param3, 0.0, "{name}: no sub-mode");
        }
    }

    #[test]
    fn px4_dotted_mode_name_resolves_like_the_short_name() {
        // The dotted decode name the state producer reports resolves to the same
        // pair as the short operator name.
        let (dotted, _b) = build_command("mode", &[json!("AUTO.MISSION")], AUTOPILOT_PX4).unwrap();
        let (short, _b2) = build_command("mode", &[json!("MISSION")], AUTOPILOT_PX4).unwrap();
        assert_eq!(dotted.param2, short.param2);
        assert_eq!(dotted.param3, short.param3);
        assert_eq!(dotted.param2, 4.0);
        assert_eq!(dotted.param3, 4.0);
    }

    #[test]
    fn mode_with_no_name_is_a_400() {
        let err = build_command("mode", &[], ARDUPILOT).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.detail, "Mode name required");
    }

    #[test]
    fn unknown_mode_name_is_a_400_on_both_families() {
        let err = build_command("mode", &[json!("NOPE")], ARDUPILOT).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.detail, "Unknown mode: NOPE");
        // A copter-only mode name is unknown on PX4.
        let err2 = build_command("mode", &[json!("POSHOLD")], AUTOPILOT_PX4).unwrap_err();
        assert_eq!(err2.detail, "Unknown mode: POSHOLD");
    }

    #[test]
    fn unknown_command_is_a_400() {
        let err = build_command("fly-to-the-moon", &[], ARDUPILOT).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.detail, "Unknown command: fly-to-the-moon");
    }

    #[test]
    fn command_serializes_to_a_valid_v2_frame() {
        // The built COMMAND_LONG serializes to a parseable v2 frame carrying the
        // same fields — proving the encode path the send uses is intact.
        let (d, _b) = build_command("mode", &[json!("RTL")], AUTOPILOT_PX4).unwrap();
        let header = MavHeader {
            system_id: SOURCE_SYSTEM_ID,
            component_id: SOURCE_COMPONENT_ID,
            sequence: 0,
        };
        let frame = mavlink::serialize_v2(header, &MavMessage::COMMAND_LONG(d)).unwrap();
        assert_eq!(frame[0], 0xFD, "a v2 frame starts with 0xFD");
        let (_h, msg) = mavlink::parse_v2(&frame).unwrap();
        match msg {
            MavMessage::COMMAND_LONG(back) => {
                assert_eq!(back.command, MavCmd::MAV_CMD_DO_SET_MODE);
                assert_eq!(back.param2, 4.0);
                assert_eq!(back.param3, 5.0);
            }
            other => panic!("expected COMMAND_LONG, got {other:?}"),
        }
    }

    #[test]
    fn catalog_has_the_six_simple_commands() {
        assert_eq!(SIMPLE_COMMANDS.len(), 6);
        let names: Vec<_> = SIMPLE_COMMANDS.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, ["arm", "disarm", "takeoff", "land", "rtl", "mode"]);
    }

    // ── G7: COMMAND_ACK correlation ─────────────────────────────────────────

    /// Serialize a COMMAND_ACK frame from `src_system` acknowledging `command`
    /// with `result`.
    fn ack_frame(src_system: u8, command: MavCmd, result: MavResult) -> Vec<u8> {
        let header = MavHeader {
            system_id: src_system,
            component_id: 1,
            sequence: 0,
        };
        mavlink::serialize_v2(
            header,
            &MavMessage::COMMAND_ACK(COMMAND_ACK_DATA { command, result }),
        )
        .unwrap()
    }

    /// Serialize a STATUSTEXT frame carrying `text`.
    fn statustext_frame(text: &str) -> Vec<u8> {
        let mut buf = [0u8; 50];
        let bytes = text.as_bytes();
        let n = bytes.len().min(50);
        buf[..n].copy_from_slice(&bytes[..n]);
        let header = MavHeader {
            system_id: TARGET_SYSTEM,
            component_id: 1,
            sequence: 0,
        };
        mavlink::serialize_v2(
            header,
            &MavMessage::STATUSTEXT(STATUSTEXT_DATA {
                severity: MavSeverity::MAV_SEVERITY_WARNING,
                text: buf,
            }),
        )
        .unwrap()
    }

    #[test]
    fn match_command_ack_correlates_on_command_and_source() {
        let frame = ack_frame(
            TARGET_SYSTEM,
            MavCmd::MAV_CMD_DO_SET_MODE,
            MavResult::MAV_RESULT_ACCEPTED,
        );
        // Matching command + source → the result.
        assert_eq!(
            match_command_ack(&frame, MavCmd::MAV_CMD_DO_SET_MODE, TARGET_SYSTEM),
            Some(0)
        );
        // Wrong command id → no match.
        assert_eq!(
            match_command_ack(&frame, MavCmd::MAV_CMD_NAV_TAKEOFF, TARGET_SYSTEM),
            None
        );
        // Wrong source system → no match.
        assert_eq!(
            match_command_ack(&frame, MavCmd::MAV_CMD_DO_SET_MODE, 42),
            None
        );
        // A non-ACK frame → no match.
        assert_eq!(
            match_command_ack(
                &statustext_frame("hi"),
                MavCmd::MAV_CMD_DO_SET_MODE,
                TARGET_SYSTEM
            ),
            None
        );
    }

    #[test]
    fn match_command_ack_reads_the_result_value() {
        let denied = ack_frame(
            TARGET_SYSTEM,
            MavCmd::MAV_CMD_COMPONENT_ARM_DISARM,
            MavResult::MAV_RESULT_DENIED,
        );
        assert_eq!(
            match_command_ack(&denied, MavCmd::MAV_CMD_COMPONENT_ARM_DISARM, TARGET_SYSTEM),
            Some(2)
        );
    }

    #[test]
    fn statustext_of_decodes_and_trims() {
        assert_eq!(
            statustext_of(&statustext_frame("PreArm: GPS")),
            Some("PreArm: GPS".to_string())
        );
        // A COMMAND_ACK is not a STATUSTEXT.
        assert_eq!(
            statustext_of(&ack_frame(
                TARGET_SYSTEM,
                MavCmd::MAV_CMD_DO_SET_MODE,
                MavResult::MAV_RESULT_ACCEPTED
            )),
            None
        );
    }

    #[test]
    fn merge_ack_writes_accepted_block() {
        let mut body = json!({"status": "ok", "cmd": "arm"});
        merge_ack(
            &mut body,
            &AckOutcome::Acked {
                result: 0,
                statustext: None,
            },
        );
        assert_eq!(body["ack"]["observed"], json!(true));
        assert_eq!(body["ack"]["result"], json!(0));
        assert_eq!(body["ack"]["result_name"], json!("ACCEPTED"));
        assert_eq!(body["ack"]["accepted"], json!(true));
        // The original body is preserved.
        assert_eq!(body["cmd"], json!("arm"));
    }

    #[test]
    fn merge_ack_writes_rejection_with_statustext() {
        let mut body = json!({"status": "ok", "cmd": "arm"});
        merge_ack(
            &mut body,
            &AckOutcome::Acked {
                result: 4,
                statustext: Some("PreArm: compass not calibrated".to_string()),
            },
        );
        assert_eq!(body["ack"]["accepted"], json!(false));
        assert_eq!(body["ack"]["result_name"], json!("FAILED"));
        assert_eq!(
            body["ack"]["statustext"],
            json!("PreArm: compass not calibrated")
        );
    }

    #[test]
    fn merge_ack_writes_unobserved_for_no_ack() {
        let mut body = json!({"status": "ok", "cmd": "arm"});
        merge_ack(&mut body, &AckOutcome::NoAck);
        assert_eq!(body["ack"]["observed"], json!(false));
        assert!(body["ack"].get("result").is_none());
    }

    /// A minimal fake FC on a unix socket: accept one client, drain its
    /// length-prefixed command frames, then (after `delay`) broadcast `responses`
    /// back framed the way the router does.
    async fn fake_fc(listener: UnixListener, responses: Vec<Vec<u8>>, delay: Duration) {
        let (stream, _addr) = listener.accept().await.unwrap();
        let (mut rd, mut wr) = stream.into_split();
        // Drain inbound command frames so the client's writes never block.
        tokio::spawn(async move {
            let mut hdr = [0u8; 4];
            loop {
                if rd.read_exact(&mut hdr).await.is_err() {
                    break;
                }
                let len = u32::from_be_bytes(hdr) as usize;
                let mut body = vec![0u8; len];
                if len > 0 && rd.read_exact(&mut body).await.is_err() {
                    break;
                }
            }
        });
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        for r in responses {
            let framed = encode_frame(&r, MAVLINK_MAX_FRAME).unwrap();
            if wr.write_all(&framed).await.is_err() {
                break;
            }
            let _ = wr.flush().await;
        }
        // Hold the write half open so the client can read before EOF.
        tokio::time::sleep(Duration::from_millis(80)).await;
    }

    fn test_cfg() -> AckConfig {
        AckConfig {
            base_timeout: Duration::from_millis(200),
            retries: 1,
            inprogress_timeout: Duration::from_millis(400),
        }
    }

    #[tokio::test]
    async fn send_awaiting_ack_returns_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mavlink.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let ack = ack_frame(
            TARGET_SYSTEM,
            MavCmd::MAV_CMD_DO_SET_MODE,
            MavResult::MAV_RESULT_ACCEPTED,
        );
        let server = tokio::spawn(fake_fc(listener, vec![ack], Duration::ZERO));

        let client = MavlinkIpcClient::new(path.clone());
        let (long, _b) = build_command("rtl", &[], AUTOPILOT_PX4).unwrap();
        let outcome = send_awaiting_ack(&client, &long, &test_cfg())
            .await
            .unwrap();
        assert_eq!(
            outcome,
            AckOutcome::Acked {
                result: 0,
                statustext: None
            }
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn send_awaiting_ack_rejection_carries_statustext() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mavlink.sock");
        let listener = UnixListener::bind(&path).unwrap();
        // A STATUSTEXT arrives, then the DENIED ack referencing our command.
        let responses = vec![
            statustext_frame("PreArm: 3D accel calibration needed"),
            ack_frame(
                TARGET_SYSTEM,
                MavCmd::MAV_CMD_COMPONENT_ARM_DISARM,
                MavResult::MAV_RESULT_DENIED,
            ),
        ];
        let server = tokio::spawn(fake_fc(listener, responses, Duration::ZERO));

        let client = MavlinkIpcClient::new(path.clone());
        let (long, _b) = build_command("arm", &[], ARDUPILOT).unwrap();
        let outcome = send_awaiting_ack(&client, &long, &test_cfg())
            .await
            .unwrap();
        assert_eq!(
            outcome,
            AckOutcome::Acked {
                result: 2,
                statustext: Some("PreArm: 3D accel calibration needed".to_string())
            }
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn send_awaiting_ack_in_progress_then_final() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mavlink.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let responses = vec![
            ack_frame(
                TARGET_SYSTEM,
                MavCmd::MAV_CMD_NAV_TAKEOFF,
                MavResult::MAV_RESULT_IN_PROGRESS,
            ),
            ack_frame(
                TARGET_SYSTEM,
                MavCmd::MAV_CMD_NAV_TAKEOFF,
                MavResult::MAV_RESULT_ACCEPTED,
            ),
        ];
        let server = tokio::spawn(fake_fc(listener, responses, Duration::ZERO));

        let client = MavlinkIpcClient::new(path.clone());
        let (long, _b) = build_command("takeoff", &[json!(15.0)], ARDUPILOT).unwrap();
        let outcome = send_awaiting_ack(&client, &long, &test_cfg())
            .await
            .unwrap();
        assert_eq!(
            outcome,
            AckOutcome::Acked {
                result: 0,
                statustext: None
            }
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn send_awaiting_ack_no_ack_is_honest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mavlink.sock");
        let listener = UnixListener::bind(&path).unwrap();
        // The FC never acks (drains commands, sends nothing).
        let server = tokio::spawn(fake_fc(listener, vec![], Duration::ZERO));

        let client = MavlinkIpcClient::new(path.clone());
        let (long, _b) = build_command("rtl", &[], ARDUPILOT).unwrap();
        let outcome = send_awaiting_ack(&client, &long, &test_cfg())
            .await
            .unwrap();
        assert_eq!(
            outcome,
            AckOutcome::NoAck,
            "no ack observed is reported honestly"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn send_awaiting_ack_ignores_an_ack_for_another_command() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mavlink.sock");
        let listener = UnixListener::bind(&path).unwrap();
        // The FC acks a DIFFERENT command; our command gets no matching ack.
        let responses = vec![ack_frame(
            TARGET_SYSTEM,
            MavCmd::MAV_CMD_NAV_LAND,
            MavResult::MAV_RESULT_ACCEPTED,
        )];
        let server = tokio::spawn(fake_fc(listener, responses, Duration::ZERO));

        let client = MavlinkIpcClient::new(path.clone());
        let (long, _b) = build_command("arm", &[], ARDUPILOT).unwrap();
        let outcome = send_awaiting_ack(&client, &long, &test_cfg())
            .await
            .unwrap();
        assert_eq!(outcome, AckOutcome::NoAck);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn send_awaiting_ack_absent_socket_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let client = MavlinkIpcClient::new(dir.path().join("absent.sock"));
        let (long, _b) = build_command("arm", &[], ARDUPILOT).unwrap();
        // No socket → Err (the route maps this to a 503), not a fake outcome.
        assert!(send_awaiting_ack(&client, &long, &test_cfg())
            .await
            .is_err());
    }
}
