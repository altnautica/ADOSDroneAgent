//! What the pilot commanded on its latest goal or stick tick, and a shadow
//! planner's command: onboard diagnostics for logs and replays.

use serde::{Deserialize, Serialize};

use super::stick::{ProfileLimits, StickCommand};

/// The frame of every position and velocity in [`PilotCommand`] and
/// [`ShadowCommand`], and of `goal_ned`, `plan_ned` and `setpoint_ned`: the
/// flight controller's local NED (metres from its EKF origin).
pub const FRAME_FC_LOCAL_NED: &str = "fc_local_ned";

/// What the pilot asked and flew on its latest control tick of a skill goal,
/// a goto or the operator's sticks, and which limits cut it since the
/// previous frame.
/// Diagnostics for logs and replays: it rides the pilot socket and the nav
/// stream, never the radio downlink.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PilotCommand {
    /// Wall-clock epoch ms of the control tick.
    pub t_ms: u64,
    /// [`FRAME_FC_LOCAL_NED`].
    pub frame: String,
    /// The skill's own goal (or the goto target) before the planner picks a
    /// detour; `goal_ned` is the goal the plan aims at.
    pub goal_ned: Option<[f64; 3]>,
    /// The point the goal aims at this tick: the goal, or the detour
    /// waypoint along `plan_ned`. While the pilot holds it is still the
    /// point the goal aims at; `None` only with no goal to aim at.
    pub target_ne: Option<[f64; 2]>,
    /// The horizontal velocity asked before any limit (m/s): for a goal
    /// whose velocity is a feed-forward, that feed-forward plus the pull
    /// toward the target; for a goal that commands its velocity, that
    /// velocity. Kept while the pilot holds, so asked against `cmd_vel_ne`
    /// shows the hold.
    pub ask_vel_ne: [f64; 2],
    /// The horizontal velocity the setpoint asks of the flight controller:
    /// its velocity plus the position-loop pull toward its position, that
    /// pull held so the sum stays within `v_cap_mps` (m/s).
    pub cmd_vel_ne: [f64; 2],
    /// The velocity field of the setpoint sent (m/s).
    pub sp_vel_ned: [f64; 3],
    /// The horizontal acceleration feed-forward of the setpoint sent (m/s²).
    #[serde(default)]
    pub sp_acc_ne: [f64; 2],
    /// The horizontal speed limit for this goal: its own maximum held to the
    /// profile's and to any `speed_cap` (m/s). Kept while the pilot holds.
    pub v_cap_mps: f64,
    /// Every limit that changed the asked velocity on any control tick since
    /// the previous frame, so a cut shorter than the frame period is still
    /// logged; empty when every tick flew as asked. The speed limit that
    /// bound comes first (`nav_cap` when the `speed_cap` is below the goal's
    /// own limit), then the rest in the order the pilot applies them. One of
    /// [`COMMAND_LIMITS`].
    pub limits: Vec<String>,
    /// Every `hold_reason` in force on a control tick of a goal since the
    /// previous frame, first seen first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hold_reasons: Vec<String>,
    /// On a stick tick: what the sticks asked and the shaped reference sent,
    /// with the input age of the frame flown. Omitted on a goal tick.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stick: Option<StickCommand>,
    /// On a stick tick: the limits the sticks flew at. Omitted on a goal
    /// tick.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_limits: Option<ProfileLimits>,
}

/// The [`PilotCommand::limits`] codes, in the order the pilot applies them:
/// `speed` (the goal's speed limit), `nav_cap` (the `speed_cap`), `sensing`
/// (the speed that stops inside the range the sensing tier sees clear along
/// the motion, [`super::Sensing`]), `shaper`
/// (the goal's acceleration and jerk limits held the setpoint's velocity
/// more than 0.05 m/s from the one asked), `hard_stop` (an obstacle inside
/// the hard-stop distance), `obstacle` (the stopping distance to an
/// obstacle), `obstacle_closing` (that limit, tightened by the scans' own
/// closing speed being faster than the velocity estimate), `seen_space` (the
/// stopping distance to where the scans stop seeing), `clearance` (the
/// clearance speed in a confined space), `push` (the push back out of the
/// margin), `subject` (the distance kept from the subject a skill's goal
/// names, `subject_ne`: the speed toward it that still stops outside that
/// distance, or the push back out of it), `geofence`, `hold` (no motion
/// toward the goal: `hold_reason` says why), `position_uncertain` (the
/// speed cap of a position whose uncertainty is wide but within the
/// pilot's limit, [`super::PilotState::pose_uncertainty_m`], applied with
/// the speed limits) and `person` (the separation kept from every person
/// or other mover the pilot tracks, [`super::PilotState::movers`]: the
/// speed toward one that still stops outside it, or the push back out of
/// it); each code's bit is its index.
pub const COMMAND_LIMITS: [&str; 15] = [
    "speed",
    "nav_cap",
    "sensing",
    "shaper",
    "hard_stop",
    "obstacle",
    "obstacle_closing",
    "seen_space",
    "clearance",
    "push",
    "subject",
    "geofence",
    "hold",
    "position_uncertain",
    "person",
];

/// The latest command of a planner the pilot runs beside the one that flies
/// (shadow mode): logged, never flown. Diagnostics like [`PilotCommand`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ShadowCommand {
    /// Which planner produced it.
    pub source: String,
    /// Wall-clock epoch ms it was produced at.
    pub t_ms: u64,
    /// Position, velocity and acceleration it would command now
    /// ([`FRAME_FC_LOCAL_NED`]; m, m/s, m/s²).
    pub pos_ne: [f64; 2],
    pub vel_ne: [f64; 2],
    pub acc_ne: [f64; 2],
    /// The path it plans from here, at most 16 points.
    pub path_ne: Vec<[f64; 2]>,
    /// Whether it flies a committed fallback rather than its exploratory plan.
    pub committed: bool,
    /// Why it commands what it does, when it says.
    pub reason: Option<String>,
    /// Whether the pilot itself flew a skill goal or a goto on its latest
    /// control tick (false: commanded, not flown, while nothing was flying).
    pub live: bool,
}

impl PilotCommand {
    /// A command at `t_ms` in [`FRAME_FC_LOCAL_NED`] with no goal, target,
    /// velocity or limit; set the other fields on the returned value.
    pub fn new(t_ms: u64) -> Self {
        Self {
            t_ms,
            frame: FRAME_FC_LOCAL_NED.to_owned(),
            goal_ned: None,
            target_ne: None,
            ask_vel_ne: [0.0; 2],
            cmd_vel_ne: [0.0; 2],
            sp_vel_ned: [0.0; 3],
            sp_acc_ne: [0.0; 2],
            v_cap_mps: 0.0,
            limits: Vec::new(),
            hold_reasons: Vec::new(),
            stick: None,
            profile_limits: None,
        }
    }
}

impl ShadowCommand {
    /// A command from `source` at `t_ms`, at rest at the origin with no
    /// path; set the other fields on the returned value.
    pub fn new(source: impl Into<String>, t_ms: u64) -> Self {
        Self {
            source: source.into(),
            t_ms,
            pos_ne: [0.0; 2],
            vel_ne: [0.0; 2],
            acc_ne: [0.0; 2],
            path_ne: Vec::new(),
            committed: false,
            reason: None,
            live: false,
        }
    }
}
