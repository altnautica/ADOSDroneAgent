//! The visor flight-action filter on the client write path.
//!
//! A node on the `visor` profile carries a flight controller as a sensor hub,
//! never as a vehicle. Every client write toward it (the direct-GCS proxies,
//! the on-box IPC socket, the aux uplink) passes through
//! [`FcConnection::visor_admit`], which on a visor forwards only whole frames
//! that are not flight actions ([`is_flight_action_frame`]) and drops the rest,
//! counted in `visor_flight_frames_dropped`. Every other profile forwards the
//! bytes unchanged.
//!
//! A flight action is a message that moves, arms or steers the vehicle or
//! drives its outputs: mode changes, mission upload and control, manual and RC
//! override input, attitude/position/actuator targets, and the `COMMAND_LONG` /
//! `COMMAND_INT` commands in [`FLIGHT_COMMANDS`]. Servo and relay commands are
//! refused too: the visor's camera trigger is driven by the flight controller's
//! own servo and camera parameters, never by a client command. Setting the home
//! position moves nothing and is allowed, as are parameter reads and writes,
//! sensor calibration, stream-rate and message requests, time sync, signing and
//! log download.
//!
//! On a visor the filter forwards nothing it has not classified. Junk between
//! frames and a trailing partial frame are not forwarded (a partial frame could
//! otherwise be completed by a later write into a frame nobody inspected), and
//! a frame whose checksum fails is skipped as the inbound framer skips it. A v2
//! frame carrying an incompatibility flag other than signing is dropped and
//! counted: the flight controller abandons such a header and would parse its
//! payload as fresh frames.

use std::borrow::Cow;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::framing::{frame_msg_id, frame_total_len, msg_id_known, STX_V1, STX_V2};
use super::FcConnection;

/// The profile whose flight controller is a sensor hub.
const VISOR_PROFILE: &str = "visor";

/// At most one drop line per this window.
const DROP_LOG_EVERY: Duration = Duration::from_secs(5);

/// MAVLink v2 incompat flag: the frame carries a signature.
const IFLAG_SIGNED: u8 = 0x01;

/// Header lengths before the payload.
const V2_HEADER_LEN: usize = 10;
const V1_HEADER_LEN: usize = 6;

const MSG_COMMAND_INT: u32 = 75;
const MSG_COMMAND_LONG: u32 = 76;

/// Offset of the `command` field in both `COMMAND_LONG` (after seven float
/// params) and `COMMAND_INT` (after four float params, `x`, `y` and `z`).
const COMMAND_OFFSET: usize = 28;

/// Message ids that are flight actions whatever their content.
const FLIGHT_MESSAGES: &[u32] = &[
    11,  // SET_MODE
    38,  // MISSION_WRITE_PARTIAL_LIST
    39,  // MISSION_ITEM
    41,  // MISSION_SET_CURRENT
    44,  // MISSION_COUNT
    45,  // MISSION_CLEAR_ALL
    69,  // MANUAL_CONTROL
    70,  // RC_CHANNELS_OVERRIDE
    73,  // MISSION_ITEM_INT
    82,  // SET_ATTITUDE_TARGET
    84,  // SET_POSITION_TARGET_LOCAL_NED
    86,  // SET_POSITION_TARGET_GLOBAL_INT
    139, // SET_ACTUATOR_CONTROL_TARGET
];

/// `MAV_CMD` values that are flight actions in `COMMAND_LONG` / `COMMAND_INT`.
pub(crate) const FLIGHT_COMMANDS: &[u16] = &[
    16,    // NAV_WAYPOINT
    17,    // NAV_LOITER_UNLIM
    18,    // NAV_LOITER_TURNS
    19,    // NAV_LOITER_TIME
    20,    // NAV_RETURN_TO_LAUNCH
    21,    // NAV_LAND
    22,    // NAV_TAKEOFF
    23,    // NAV_LAND_LOCAL
    24,    // NAV_TAKEOFF_LOCAL
    25,    // NAV_FOLLOW
    31,    // NAV_LOITER_TO_ALT
    34,    // DO_ORBIT
    84,    // NAV_VTOL_TAKEOFF
    85,    // NAV_VTOL_LAND
    92,    // NAV_GUIDED_ENABLE
    115,   // CONDITION_YAW
    176,   // DO_SET_MODE
    177,   // DO_JUMP
    178,   // DO_CHANGE_SPEED
    181,   // DO_SET_RELAY
    182,   // DO_REPEAT_RELAY
    183,   // DO_SET_SERVO
    184,   // DO_REPEAT_SERVO
    185,   // DO_FLIGHTTERMINATION
    186,   // DO_CHANGE_ALTITUDE
    188,   // DO_RETURN_PATH_START
    189,   // DO_LAND_START
    191,   // DO_GO_AROUND
    192,   // DO_REPOSITION
    193,   // DO_PAUSE_CONTINUE
    194,   // DO_SET_REVERSE
    208,   // DO_PARACHUTE
    209,   // DO_MOTOR_TEST
    211,   // DO_GRIPPER
    218,   // DO_AUX_FUNCTION
    223,   // DO_ENGINE_CONTROL
    224,   // DO_SET_MISSION_CURRENT
    300,   // MISSION_START
    400,   // COMPONENT_ARM_DISARM
    42600, // DO_WINCH
];

/// Whether a frame with `msg_id` and (possibly zero-truncated v2) `payload` is
/// a flight action.
pub(crate) fn is_flight_action_frame(msg_id: u32, payload: &[u8]) -> bool {
    match msg_id {
        MSG_COMMAND_LONG | MSG_COMMAND_INT => FLIGHT_COMMANDS.contains(&command_of(payload)),
        id => FLIGHT_MESSAGES.contains(&id),
    }
}

/// The `command` field of a `COMMAND_LONG` / `COMMAND_INT` payload. A v2
/// sender trims trailing zero bytes, so a missing byte reads as zero.
fn command_of(payload: &[u8]) -> u16 {
    let byte = |i: usize| payload.get(i).copied().unwrap_or(0);
    u16::from_le_bytes([byte(COMMAND_OFFSET), byte(COMMAND_OFFSET + 1)])
}

/// What the filter does with the bytes at the cursor.
enum Step {
    /// Forward this many bytes (one whole permitted frame).
    Keep(usize),
    /// Do not forward this many bytes (junk, a bad checksum, a partial tail).
    Skip(usize),
    /// Drop this many bytes and count them (one refused frame).
    Refuse(usize),
}

/// The verdict on one whole candidate frame.
fn classify(frame: &[u8]) -> Step {
    let len = frame.len();
    let v2 = frame[0] == STX_V2;
    if v2 && frame[2] & !IFLAG_SIGNED != 0 {
        return Step::Refuse(len);
    }
    let Some(msg_id) = frame_msg_id(frame) else {
        return Step::Skip(1);
    };
    if msg_id_known(msg_id) && !ados_protocol::mavlink::frame_checksum_ok(frame) {
        return Step::Skip(1);
    }
    let header = if v2 { V2_HEADER_LEN } else { V1_HEADER_LEN };
    let payload = &frame[header..header + usize::from(frame[1])];
    if is_flight_action_frame(msg_id, payload) {
        Step::Refuse(len)
    } else {
        Step::Keep(len)
    }
}

/// `data` with every flight-action frame removed, and how many frames were
/// refused. Only whole permitted frames are kept (see the module docs).
/// Borrows `data` when it is forwarded whole.
pub(crate) fn strip_flight_actions(data: &[u8]) -> (Cow<'_, [u8]>, u64) {
    let mut out: Option<Vec<u8>> = None;
    let mut refused = 0u64;
    let mut i = 0usize;
    while i < data.len() {
        let rest = &data[i..];
        let step = match frame_total_len(rest) {
            Some(total) if rest.len() >= total => classify(&rest[..total]),
            // A start-of-frame with too few bytes to finish: only a later
            // write could complete it.
            Some(_) => Step::Skip(rest.len()),
            None if rest[0] == STX_V2 || rest[0] == STX_V1 => Step::Skip(rest.len()),
            None => Step::Skip(1),
        };
        match step {
            Step::Keep(n) => {
                if let Some(out) = out.as_mut() {
                    out.extend_from_slice(&rest[..n]);
                }
                i += n;
            }
            Step::Skip(n) => {
                out.get_or_insert_with(|| data[..i].to_vec());
                i += n;
            }
            Step::Refuse(n) => {
                refused += 1;
                out.get_or_insert_with(|| data[..i].to_vec());
                i += n;
            }
        }
    }
    (out.map_or(Cow::Borrowed(data), Cow::Owned), refused)
}

impl FcConnection {
    /// Whether this router runs on a visor.
    pub(crate) fn is_visor(&self) -> bool {
        self.cfg.profile == VISOR_PROFILE
    }

    /// The bytes of a client write that may reach the flight controller: on a
    /// visor `data` without its flight-action frames, elsewhere `data`
    /// unchanged.
    pub(crate) fn visor_admit<'a>(&self, data: &'a [u8]) -> Cow<'a, [u8]> {
        if !self.is_visor() {
            return Cow::Borrowed(data);
        }
        let (kept, refused) = strip_flight_actions(data);
        self.note_visor_refusals(refused);
        kept
    }

    /// Count `refused` flight-action frames dropped on a visor, logging at
    /// most once per [`DROP_LOG_EVERY`].
    pub(crate) fn note_visor_refusals(&self, refused: u64) {
        if refused == 0 {
            return;
        }
        let total = self
            .visor_flight_frames_dropped
            .fetch_add(refused, Ordering::Relaxed)
            + refused;
        // Nanoseconds since the connection epoch, offset by one so zero means
        // never logged.
        let now = u64::try_from(self.epoch.elapsed().as_nanos()).unwrap_or(u64::MAX) | 1;
        let last = self.visor_drop_logged_at.load(Ordering::Relaxed);
        let due = last == 0 || now.saturating_sub(last) >= DROP_LOG_EVERY.as_nanos() as u64;
        if due
            && self
                .visor_drop_logged_at
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            tracing::debug!(refused, total, "visor_flight_frames_dropped");
        }
    }

    /// Cumulative flight-action frames dropped because this node is a visor.
    pub fn visor_flight_frames_dropped(&self) -> u64 {
        self.visor_flight_frames_dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests;
