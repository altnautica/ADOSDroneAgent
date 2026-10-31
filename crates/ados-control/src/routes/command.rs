//! Command routes: the text-command executor + the command catalog.
//!
//! `POST /api/command` is the GCS's control path: it names one of a small fixed
//! set of high-level commands (`arm`, `disarm`, `takeoff`, `land`, `rtl`,
//! `mode`, `killSwitch`, `pauseMission`, `resumeMission`), and the route turns
//! that into the corresponding MAVLink frame, writes it to
//! `/run/ados/mavlink.sock` (which the router forwards to the FC), and correlates
//! the flight controller's `COMMAND_ACK` so the response reports whether the FC
//! accepted the command. `GET /api/commands` returns the catalog of those names
//! with descriptions. The command name match is case-insensitive (the route
//! lowercases `cmd` before dispatch), so the fleet board's camelCase action names
//! resolve to the same frame.
//!
//! ## How commands reach the FC
//!
//! The native router owns the FC serial link, so there is no in-process
//! MAVLink connection to command through. This route builds the MAVLink frame
//! itself and writes it to the router's MAVLink socket. Each frame carries the
//! standard pymavlink encoding of the command (`arducopter_arm()`,
//! `command_long_send(...)`, `set_mode_apm(...)`).
//!
//! ## What each command maps to
//!
//! Every command maps to a `COMMAND_LONG`. `arm`/`disarm`/`takeoff`/`land` are
//! autopilot-agnostic (the same `MAV_CMD` and params on ArduPilot and PX4).
//! `rtl`/`mode` set a flight mode via `MAV_CMD_DO_SET_MODE` (176), and the mode
//! encoding differs by autopilot family:
//!
//! | cmd           | command opcode                  | params (1..7)                    |
//! |---------------|---------------------------------|----------------------------------|
//! | arm           | MAV_CMD_COMPONENT_ARM_DISARM 400| p1=1.0, rest 0                   |
//! | disarm        | MAV_CMD_COMPONENT_ARM_DISARM 400| p1=0.0, rest 0                   |
//! | takeoff       | MAV_CMD_NAV_TAKEOFF 22          | p7=alt (default 10.0), rest 0    |
//! | land          | multicopter / PX4: MAV_CMD_NAV_LAND 21, all 0 · ArduPlane QuadPlane: DO_SET_MODE QLAND (20) · ArduPlane fixed-wing: MAV_CMD_DO_LAND_START 189, all 0 |
//! | rtl           | MAV_CMD_DO_SET_MODE 176         | ArduPilot: p1=1, p2=RTL of the vehicle's table (Copter 6, Plane/Rover 11) · PX4: p1=1, p2=4, p3=5 |
//! | mode N        | MAV_CMD_DO_SET_MODE 176         | ArduPilot: p1=1, p2=custom_mode · PX4: p1=1, p2=main, p3=sub |
//! | killSwitch    | MAV_CMD_COMPONENT_ARM_DISARM 400| p1=0.0, p2=21196 (force), rest 0 |
//! | pauseMission  | MAV_CMD_DO_PAUSE_CONTINUE 193   | p1=0.0 (pause), rest 0           |
//! | resumeMission | MAV_CMD_DO_PAUSE_CONTINUE 193   | p1=1.0 (continue), rest 0        |
//!
//! ## killSwitch is a FORCE-DISARM, not a graceful stop
//!
//! `killSwitch` is an emergency motor cut: `MAV_CMD_COMPONENT_ARM_DISARM` with
//! `param1=0` (disarm) and `param2=21196` — the MAVLink "force" magic that both
//! ArduPilot and PX4 honour to disarm IMMEDIATELY, bypassing the normal
//! land-detector / in-flight safety checks a plain `disarm` respects. The honest
//! effect: the motors stop at once, so an airborne vehicle drops. It is the
//! last-resort abort, distinct from `disarm` (which the FC refuses in flight) and
//! from `rtl`/`land` (which fly the vehicle down under power). The catalog
//! description says so plainly.
//!
//! ## pauseMission / resumeMission
//!
//! `pauseMission` and `resumeMission` both send `MAV_CMD_DO_PAUSE_CONTINUE` (193)
//! — the one autopilot-agnostic pause/continue command ArduPilot and PX4 share —
//! with `param1=0` to pause (hold position / brake) and `param1=1` to continue.
//! They are a pair over one opcode exactly as `arm`/`disarm` are over
//! `COMPONENT_ARM_DISARM`. A vehicle that is not running an auto mission will
//! report the FC's own rejection in the `ack` block (honest, never masked).
//!
//! ## Flight-mode encoding: ArduPilot vs PX4
//!
//! ArduPilot carries a flat `custom_mode` integer, so `DO_SET_MODE` sets
//! `param1=1` (custom-mode enabled) and `param2=custom_mode`, `param3=0`. The
//! number is firmware-specific (`6` is RTL on Copter, FBWB on Plane, FOLLOW on
//! Rover), so the name resolves through the table for the vehicle type the FC
//! reports in `HEARTBEAT.type` — the same shared table the router decodes the
//! snapshot's mode name with. A vehicle type with no table (unknown, generic,
//! submarine, ...) is refused with a 409 instead of guessing a number.
//!
//! PX4 uses a two-level `(main_mode, sub_mode)` scheme. For the `DO_SET_MODE`
//! **command**, PX4 reads the two levels as SEPARATE small integers:
//! `param2 = main_mode`, `param3 = sub_mode` — it does NOT read a bit-packed
//! value from `param2`. (The `main << 16 | sub << 24` packing is the layout of
//! the 32-bit `custom_mode` FIELD in `HEARTBEAT`/`SET_MODE`, which the state
//! producer DECODES to name the current mode; the command layer takes the two
//! levels de-packed.) So on PX4, RTL is the AUTO main mode with the RTL sub mode
//! (`param2=4`, `param3=5`).
//!
//! The route reads the FC's advertised autopilot and vehicle type from the live
//! state snapshot (`autopilot == 12` is PX4) and picks the encoding accordingly.
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
//! - **Target**: the flight controller's own `(system, component)` as resolved
//!   per request by [`crate::fc_target::fc_target`] (the system id latched from
//!   its HEARTBEAT). The ACK's source system is correlated against this target
//!   so an ACK from the addressed vehicle is the one that resolves.

use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use ados_protocol::flight_modes::{ArduPilotFirmware, MAV_AUTOPILOT_PX4};
use ados_protocol::mavlink::ardupilotmega::{MavCmd, MavMessage, COMMAND_LONG_DATA};
use ados_protocol::mavlink::{self, MavHeader};

use crate::ipc::{FrameRead, MavlinkIpcClient};
use crate::routes::detail;
use crate::state::{AppState, FcIdentity};

mod ack;
mod build;
use ack::*;
use build::*;

/// The command catalog: name → human description. The first six are the original
/// text commands; the last three are agent-native additions the fleet board
/// drives (kill / pause / resume). Order is preserved (it is emitted as a JSON
/// object; serde_json keeps insertion order). The names are the canonical strings
/// a caller sends; the dispatch match is case-insensitive.
const SIMPLE_COMMANDS: &[(&str, &str)] = &[
    ("arm", "Arm the vehicle"),
    ("disarm", "Disarm the vehicle"),
    ("takeoff", "Takeoff to altitude (args: [altitude_m])"),
    ("land", "Land at current position"),
    ("rtl", "Return to launch"),
    ("mode", "Set flight mode (args: [mode_name])"),
    (
        "killSwitch",
        "Emergency motor cut: force-disarm now, bypassing in-flight safety checks. \
         Stops the motors immediately, so an airborne vehicle drops.",
    ),
    (
        "pauseMission",
        "Pause the current mission / auto flight (hold position)",
    ),
    ("resumeMission", "Resume a paused mission / auto flight"),
];

/// The source identity stamped on every command frame: the agent/companion
/// identity the router uses on its own FC send path (defaults 1/191), so a
/// command from this surface is wire-identical to one the router sent.
const SOURCE_SYSTEM_ID: u8 = 1;
const SOURCE_COMPONENT_ID: u8 = 191;

/// Default takeoff altitude in metres when the request carries no `args[0]`.
const DEFAULT_TAKEOFF_ALT_M: f32 = 10.0;

/// The upper bound the takeoff route accepts, in metres above the takeoff point.
///
/// 1000 m AGL is above every civil altitude limit, so a larger number is a unit
/// or typing error (feet entered as metres, a trailing digit) rather than an
/// intent — and `param7` reaches the FC as the climb target either way. Refusing
/// it names the mistake while the aircraft is still on the ground.
const MAX_TAKEOFF_ALT_M: f32 = 1000.0;

/// The custom-mode flag the DO_SET_MODE param1 carries, matching
/// `MAV_MODE_FLAG_CUSTOM_MODE_ENABLED` (1).
const CUSTOM_MODE_ENABLED: f32 = 1.0;

/// The `COMPONENT_ARM_DISARM` `param2` "force" magic (21196). With this value
/// both ArduPilot and PX4 disarm IMMEDIATELY, bypassing the in-flight / land-
/// detector safety checks a plain disarm (`param2=0`) respects. `killSwitch`
/// sends it to cut the motors as a last-resort abort — the vehicle drops if
/// airborne, which the catalog description states plainly.
const FORCE_DISARM_MAGIC: f32 = 21196.0;

/// `DO_PAUSE_CONTINUE` `param1`: pause the current action (0) or continue it (1).
const PAUSE_CONTINUE_PAUSE: f32 = 0.0;
const PAUSE_CONTINUE_CONTINUE: f32 = 1.0;

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

/// PX4 flight-mode name → `(main_mode, sub_mode)` for the `DO_SET_MODE` command
/// path. Matches the router's PX4 decode table, with both the short operator
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

/// The `POST /api/command` request body. `args` defaults to an empty list when omitted.
#[derive(Debug, Deserialize)]
pub struct CommandRequest {
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<Value>,
}

/// `POST /api/command` → `{"status":"ok","cmd":<cmd>, "ack": {...}}` when the FC
/// ACCEPTED it, else `{"status":"error","message":<why>, ...}` with the same `ack`
/// block (a rejection, an unfinished IN_PROGRESS, or no ACK at all).
///
/// 503 `{"detail": ...}` when the FC is not connected OR the MAVLink socket
/// cannot be reached (the command never silently drops). 400 `{"detail": ...}`
/// on an unknown command, a `mode` with no name, or an unknown mode name. 409
/// on `rtl`/`mode` for an ArduPilot vehicle type with no mode table.
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

    // An MSP flight controller cannot be commanded from this route.
    //
    // Everything below builds a MAVLink COMMAND_LONG. A Betaflight or iNav
    // board never heartbeats, so `autopilot()` falls through to its `0` default
    // and is treated as ArduPilot — the frame is then written to the serial
    // port and silently discarded by an FC that does not speak MAVLink, while
    // the operator is told the command was sent. Refuse by name instead: the
    // agent already knows the FC is MSP and the status route already says so.
    if state.fc_speaks_msp() {
        return detail(
            StatusCode::NOT_IMPLEMENTED,
            "this flight controller speaks MSP (Betaflight/iNav); MAVLink \
             commands do not apply to it",
        );
    }

    let cmd = req.cmd.to_lowercase();
    let fc = state.fc_identity();

    // Build the COMMAND_LONG + the success body for the named command. An unknown
    // command (or a bad mode arg) returns a 4xx here before any send.
    let (base_long, mut body) = match build_command(&cmd, &req.args, fc) {
        Ok(built) => built,
        Err(err) => return err.into_response(),
    };
    // Address it to the flight controller this request resolved: the system id
    // latched from its HEARTBEAT. The ACK is correlated against the same id.
    let base_long = addressed(base_long, crate::fc_target::fc_target(&state));

    // Send the command and correlate the FC's COMMAND_ACK on a dedicated socket
    // connection. An absent MAVLink socket or a broken write means no live FC
    // link → 503.
    match send_awaiting_ack(&state.mavlink, &base_long, &AckConfig::default()).await {
        Ok(outcome) => {
            merge_ack(&mut body, &outcome);
            settle_status(&mut body, &outcome);
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, cmd = %cmd, "command send/ack exchange failed");
            detail(StatusCode::SERVICE_UNAVAILABLE, "No MAVLink connection")
        }
    }
}

/// A 4xx the command builder raises before any send: an unknown command, a `mode`
/// with no name, an unknown mode name, or a mode change for a vehicle type with
/// no mode table.
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

/// `GET /api/commands` → `{"commands": {name: description, ...}}`. The catalog,
/// in its fixed wire order.
pub async fn list_commands() -> Json<Value> {
    let mut commands = serde_json::Map::new();
    for (name, desc) in SIMPLE_COMMANDS {
        commands.insert((*name).to_string(), json!(desc));
    }
    Json(json!({ "commands": commands }))
}

#[cfg(test)]
mod tests;
