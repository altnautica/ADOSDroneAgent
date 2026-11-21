//! The pilot's published state: the nav-stream `pilot` and `obstacles` kinds.

use serde::{Deserialize, Serialize};

use super::command::{PilotCommand, ShadowCommand};
use super::rc::RcReceiver;
use super::reserve::{BatteryReserve, OverheadAhead};
use super::route::{PlaceClass, RouteCheck};
use super::stick::{ProfileLimits, StickCommand};

/// Who currently commands motion.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Owner {
    #[default]
    Idle,
    Pilot,
    Skill,
    Goto,
    Swarm,
    MissionFc,
    FcMode,
    Tuning,
}

/// Where operator sticks go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlMode {
    #[default]
    Ados,
    FcManual,
    Elrs,
}

impl ControlMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ados => "ados",
            Self::FcManual => "fc_manual",
            Self::Elrs => "elrs",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ados" => Some(Self::Ados),
            "fc_manual" => Some(Self::FcManual),
            "elrs" => Some(Self::Elrs),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    Cine,
    #[default]
    Normal,
    Sport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AvoidMode {
    #[default]
    Brake,
    Bypass,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AvoidState {
    #[default]
    Clear,
    Detour,
    Blocked,
    Off,
    /// Avoidance is on and a lidar was seen, but its scans are stale:
    /// obstacles are unknown, so the pilot holds and flies no motion command
    /// until the scans are fresh.
    Stale,
    /// Avoidance is on but no obstacle data was ever seen (no lidar fitted
    /// or detected): nothing is avoided, sticks fly, and features that need
    /// obstacle data are refused.
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VehicleClass {
    #[default]
    Multirotor,
    Fixedwing,
    Vtol,
}

/// The skill that holds the motion lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRef {
    pub plugin_id: String,
    pub skill_id: String,
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Block,
    Warn,
}

/// One pre-flight readiness item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Readiness {
    pub id: String,
    pub ok: bool,
    pub severity: Severity,
    pub msg: String,
}

/// FC mission state, computed by the router (`mission` of the vehicle state)
/// and copied here. `seq`, `total` and `age_ms` are null until the FC has
/// reported MISSION_CURRENT, as on the router's wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mission {
    pub state: String,
    pub seq: Option<u32>,
    pub total: Option<u32>,
    pub item_reached: Option<u32>,
    pub id: Option<u32>,
    pub source: String,
    pub age_ms: Option<u64>,
    pub landing_seq: bool,
    /// The active item's target, from the router's mirror at `seq`: the item
    /// position in the FC's local NED frame when it has one. Omitted while
    /// the mirror holds no item at `seq`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<MissionTarget>,
    /// Items the pilot skipped this mission (coverage gaps to re-fly),
    /// oldest first. Omitted while empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<MissionGap>,
}

/// The mission item the FC flies now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissionTarget {
    pub seq: u32,
    /// MAV_CMD of the item.
    pub cmd: u16,
    /// The item position in the FC's local NED frame, metres; `None` for an
    /// item without a position (DELAY, DO_*), a zero position (hold here), a
    /// terrain-relative altitude or before the vehicle's own global position
    /// is known.
    #[serde(default)]
    pub ned: Option<[f64; 3]>,
    /// A loiter item's radius, metres (`None` for other items).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radius_m: Option<f64>,
}

/// One skipped mission item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionGap {
    pub seq: u32,
    /// `stall_timeout` (skipped after the countdown) or `stall_operator`.
    pub reason: String,
    /// Wall-clock epoch ms of the skip.
    pub at_ms: u64,
}

/// A mission or return that stopped making progress: the vehicle holds and
/// the operator chooses. Persistent while it lasts (the banner is a 3 s slot).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissionStall {
    /// `mission` (no progress to a waypoint) or `return` (a blocked return).
    pub kind: String,
    /// The mission item stalled on; `None` for a return.
    #[serde(default)]
    pub seq: Option<u32>,
    /// The item's label as the plan shows it; `None` for a return.
    #[serde(default)]
    pub wp_label: Option<String>,
    /// Wall-clock epoch ms the stall was raised.
    pub since_ms: u64,
    /// Seconds until the automatic action: the skip for a mission, the
    /// landing for a return. `None`: no countdown, the vehicle holds until
    /// the operator answers.
    #[serde(default)]
    pub skip_in_s: Option<f64>,
    /// Seconds until the vehicle returns home along its flown path
    /// (SmartRTL) on its own: a mission held for the operator in a confined
    /// place that no operator has answered. On a position the pilot does not
    /// vouch for at that moment it lands in place instead. `None`: no such
    /// countdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_in_s: Option<f64>,
    /// The no-progress window that raised it, seconds.
    pub window_s: f64,
}

/// The surface whose sticks are accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputSession {
    pub src: String,
    pub session: u32,
    pub label: String,
}

/// The centred-sticks handover gate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Gate {
    pub pending: bool,
    pub progress: f64,
}

/// Progress of a `launch` sequence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sequence {
    /// `checks`, `arm`, `takeoff`, `after` or `done`.
    pub step: String,
    /// `hold`, `skill`, `mission` or `goto`.
    pub after: String,
    pub error: Option<String>,
    /// The supervised lift-off, from its spool-up until it hands over in
    /// the air (or stops on the ground); omitted while the launch runs none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub liftoff: Option<Liftoff>,
}

/// Steady take-off: the pilot balances the airframe on the ground with its
/// trim loaded into the FC's rate loop, then lifts off level.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Liftoff {
    /// `spool` (motors spinning up), `level` (loading the learned trim on
    /// the ground), `probe` (finding the trim on the ground: first flight
    /// on this airframe), `lift`, `climb` (clear of the ground, level),
    /// `done` (handed over) or `stopped` (stopped on the ground; `detail`
    /// says why).
    pub phase: String,
    /// The phase's status line, e.g. "Spinning up", "Checking balance",
    /// "Take-off stopped".
    pub text: String,
    /// A longer line for surfaces with room, e.g. "Climbing to 3 m" or
    /// "The drone started to tip. Motors off."; empty when the status line
    /// says it all.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// `learned` (the trim this airframe flew with before) or `probe`.
    pub trim: String,
    /// Tilt from the resting attitude, degrees.
    pub tilt_deg: f64,
}

/// A transient full-width announcement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Banner {
    pub text_key: String,
    pub args: serde_json::Map<String, serde_json::Value>,
    pub until_ms: u64,
}

/// One FC STATUSTEXT at WARNING or worse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FcStatusText {
    /// MAVLink severity: 0 emergency … 4 warning.
    pub severity: u8,
    pub text: String,
    pub age_ms: u64,
}

/// A horizontal speed cap the pilot enforces on every setpoint it flies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SpeedCap {
    /// Why: `gnss-denied` while the EKF's horizontal position does not come
    /// from GNSS; `confined-near` in a confined place while an obstacle is
    /// near the clearance limit or the mission leg flown is not yet seen.
    pub reason: String,
    /// The horizontal speed limit in m/s.
    pub v_h_mps: f32,
}

impl SpeedCap {
    pub fn new(reason: impl Into<String>, v_h_mps: f32) -> Self {
        Self {
            reason: reason.into(),
            v_h_mps,
        }
    }
}

/// The pilot does not trust the EKF's horizontal position: the Navigation
/// external position is degraded or lost, or the flight controller's optical
/// flow says the vehicle moves when the estimate says otherwise. While set,
/// no hold is a position hold and nothing new is flown on the estimate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PositionUnverified {
    /// `flow_disagrees` (the pilot's own flow-against-estimate check),
    /// `extnav_lost` or `extnav_degraded`.
    pub reason: String,
    /// Navigation's `health_reason` (`corridor_unmeasured`, `flow_disagrees`,
    /// `ekf_flow_disagree`, …) when it named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Horizontal speed over ground from the optical flow, m/s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_mps: Option<f32>,
    /// Horizontal speed of the EKF estimate, m/s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate_mps: Option<f32>,
}

impl PositionUnverified {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            detail: None,
            flow_mps: None,
            estimate_mps: None,
        }
    }

    pub fn with_detail(mut self, detail: Option<String>) -> Self {
        self.detail = detail;
        self
    }

    pub fn with_speeds(mut self, flow_mps: f32, estimate_mps: f32) -> Self {
        self.flow_mps = Some(flow_mps);
        self.estimate_mps = Some(estimate_mps);
        self
    }
}

/// The nav-stream `pilot` kind with the handover, launch and banner fields.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PilotState {
    pub owner: Owner,
    pub owner_label: String,
    pub skill: Option<SkillRef>,
    pub vehicle_class: VehicleClass,
    pub control_mode: ControlMode,
    pub profile: Profile,
    pub avoid: AvoidMode,
    /// The FC's custom mode; `None` (JSON `null`) while the FC link is down,
    /// so a surface shows the mode as unknown.
    pub fc_mode: Option<u32>,
    /// The FC's mode name; `None` (JSON `null`) while the FC link is down.
    pub fc_mode_name: Option<String>,
    /// Whether the FC reports armed; `None` (JSON `null`) while the FC link
    /// is down, so a surface treats the vehicle as possibly airborne.
    pub armed: Option<bool>,
    /// Whether the FC reports landed; `None` (JSON `null`) while the FC link
    /// is down.
    pub landed: Option<bool>,
    /// A VTOL between hover and forward flight (the FC's EXTENDED_SYS_STATE
    /// says it is transitioning either way).
    pub vtol_transitioning: bool,
    pub input_src: Option<String>,
    pub input_age_ms: Option<u64>,
    pub hold_reason: Option<String>,
    pub avoid_state: AvoidState,
    pub min_clearance_m: Option<f64>,
    pub goal_ned: Option<[f64; 3]>,
    pub plan_ned: Vec<[f64; 2]>,
    pub setpoint_ned: Option<[f64; 3]>,
    pub readiness: Vec<Readiness>,
    pub link_loss_in_s: Option<f64>,
    /// What the pilot does when the control link stays lost: `rth`, `hold`
    /// or `land` (the configured `pilot.link_loss_action`).
    pub link_loss_action: String,
    /// The link-loss window, seconds (the configured `pilot.link_loss_s`):
    /// how long the pilot waits for an operator frame before
    /// `link_loss_action`.
    #[serde(default)]
    pub link_loss_s: u32,
    /// The operator's RC declaration; omitted while `present`.
    #[serde(default, skip_serializing_if = "RcReceiver::is_present")]
    pub rc_receiver: RcReceiver,
    pub mission: Option<Mission>,
    pub input_session: Option<InputSession>,
    pub gate: Option<Gate>,
    pub sequence: Option<Sequence>,
    pub banner: Option<Banner>,
    pub rth_countdown_s: Option<f64>,
    /// A mission or return that stopped making progress (the vehicle holds
    /// for the operator); omitted while none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission_stall: Option<MissionStall>,
    /// The operator's place class; omitted while `open` (an absent field
    /// reads as `open`).
    #[serde(default, skip_serializing_if = "PlaceClass::is_open")]
    pub place_class: PlaceClass,
    /// The mission route check, while the place is confined and the FC
    /// holds a mission with at least one leg; omitted otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_check: Option<RouteCheck>,
    /// Pilot actions a ground surface may send on the pilot stream beyond
    /// the safety set (e.g. `goto`).
    pub surface_ops: Vec<String>,
    /// Height above ground in metres, `None` without a source.
    pub agl_m: Option<f64>,
    /// `rangefinder` (fresh downward reading) or `home` (EKF height above
    /// the take-off point).
    pub agl_source: Option<String>,
    /// FC STATUSTEXT at WARNING or worse from the last 30 s, newest first.
    pub statustext: Vec<FcStatusText>,
    /// The flight controller flies an RTL or land it entered by itself (an RC
    /// switch, or a radio, battery or EKF failsafe): no client or pilot mode
    /// command preceded it. A reposition waits until the operator changes
    /// mode. Omitted while false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fc_failsafe: bool,
    /// Why the FC flies `fc_failsafe`, as the `pilot.banner.cause.<code>`
    /// code: `rc_or_failsafe` (the FC reports RC input) or `fc`. `None`
    /// unless `fc_failsafe`.
    #[serde(default)]
    pub failsafe_cause: Option<String>,
    /// The pilot's own low-battery action: `land` (the vehicle may fly
    /// without GNSS, so it lands where it is) or `rth`. Set while its
    /// countdown runs (`rth_countdown_s`) and after it fired, until the
    /// battery recovers or the vehicle is on the ground. Omitted while none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub low_battery_action: Option<String>,
    /// The charge a low-battery return home needs, every tick airborne;
    /// omitted on the ground.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_reserve: Option<BatteryReserve>,
    /// The leaves overhead along the way to a skill goal's subject, while a
    /// lease goal names one ([`OverheadAhead`]); omitted otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overhead_ahead: Option<OverheadAhead>,
    /// The FC setup engine's active step id (`autotune`, `first_hover`, …)
    /// while the owner is `tuning`; `None` otherwise.
    #[serde(default)]
    pub setup_step: Option<String>,
    /// The `setup.blocker.aborted_<reason>` key of the air wizard's last
    /// abort that was not the operator's, for a while after it; `None`
    /// otherwise.
    #[serde(default)]
    pub setup_abort: Option<String>,
    /// The horizontal speed cap in force, omitted while there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed_cap: Option<SpeedCap>,
    /// The sensing tier, the seen-clear range along the vehicle's motion and
    /// the speed limit it allows; omitted while no obstacle sensor is seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensing: Option<super::Sensing>,
    /// Flow, downward range and lidar health; `None` (JSON `null`) while the
    /// FC link is down.
    #[serde(default)]
    pub sensors: Option<super::SensorHealth>,
    /// The horizontal position is not trusted, omitted while it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position_unverified: Option<PositionUnverified>,
    /// The pilot's latest command of a skill goal or a goto, omitted while
    /// it flies none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<PilotCommand>,
    /// The shadow planner's latest command, omitted when there is none or it
    /// is older than a second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow: Option<ShadowCommand>,
    /// The limits the operator's sticks fly at now; omitted when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_limits: Option<ProfileLimits>,
    /// The operator's sticks on the latest stick tick, while they own the
    /// vehicle; omitted otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stick: Option<StickCommand>,
    /// The clock the pilot runs on, stamped by the runtime on every state
    /// message; omitted only by producers that do not stamp it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<super::ClockStamp>,
}
