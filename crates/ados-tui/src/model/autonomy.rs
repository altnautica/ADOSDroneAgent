//! The autonomy readings from `GET /api/v1/nav/status`: who flies the vehicle
//! (the `pilot` kind), the follow state, the navigation source, the lidar and
//! the live map session; and, in [`detection`], the detector's per-camera
//! state from `GET /api/vision/status`.
//!
//! The nav route answers each kind as `{ts_ms, age_ms, data}`, or `null` when
//! no plugin has published it, so every part here is optional and nothing is
//! filled with a default.

use serde_json::Value;

use super::flight_state::stale_after_ms;
use super::route_check::RouteCheck;
use super::sensor_health::SensorHealth;
use super::spec_text::spec_text;

pub mod detection;
mod eta;
mod tone;
use eta::next_waypoint_eta_s;
mod pilot_kind;
use pilot_kind::pilot;
pub use tone::{health_tone, Tone};

/// The path the Vehicle tab polls.
pub const NAV_STATUS_PATH: &str = "/api/v1/nav/status";

/// The steps of the pilot's launch sequence.
const LAUNCH_STEPS: [&str; 5] = ["checks", "arm", "takeoff", "after", "done"];

/// The mission states the pilot kind reports.
const MISSION_STATES: [&str; 6] = [
    "none",
    "loaded",
    "active",
    "paused",
    "complete",
    "returning",
];

fn f(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64).filter(|x| x.is_finite())
}

fn i(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(Value::as_i64)
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A string field holding one of `values`, else `None`.
fn one_of(v: &Value, key: &str, values: &[&str]) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| values.contains(s))
        .map(str::to_string)
}

/// One kind's payload, its age, and whether it is stale: at least the spec's
/// `stale_after_ms` old. A kind without a whole-millisecond `age_ms` is of
/// unknown age, so it is stale too.
fn kind<'a>(v: &'a Value, name: &str) -> Option<(&'a Value, Option<u64>, bool)> {
    let entry = v.get(name).filter(|e| !e.is_null())?;
    let data = entry.get("data")?;
    let age = entry.get("age_ms").and_then(Value::as_u64);
    let stale = age.is_none_or(|a| a >= stale_after_ms());
    Some((data, age, stale))
}

/// The follow-me read-back (`follow.state`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Follow {
    pub phase: Option<String>,
    pub track_id: Option<i64>,
    pub range_m: Option<f64>,
    pub range_sigma_m: Option<f64>,
    pub reacquire_mode: Option<String>,
    /// Seconds left of the recovery's search budget.
    pub reacquire_remaining_s: Option<f64>,
    /// The 0-based widening step while searching.
    pub search_step: Option<i64>,
    /// What hides the subject: `narrow` (a trunk, a column) or `wide` (a
    /// wall, a corner).
    pub occluder_kind: Option<String>,
    /// The waiting prompt's title while follow holds for the operator.
    pub prompt_title: Option<String>,
    /// Why follow holds (`camera-lost`, `search-exhausted`, ...).
    pub hold_reason: Option<String>,
    /// Height above ground the follow skill flies on (`rng` or `baro`).
    pub agl_m: Option<f64>,
    pub agl_source: Option<String>,
    /// The follow height flown while it is lowered under leaves
    /// (`height_setpoint_m` with `height_lowered`); `None` while flown as set.
    pub height_lowered_m: Option<f64>,
    /// Why the height is lowered (`canopy`, `canopy_floor`), when named.
    pub height_reason: Option<String>,
    /// The kind's age at the agent when read; `None` when it reports none.
    pub age_ms: Option<u64>,
    pub stale: bool,
}

/// The waiting prompt's title for a `follow.state` payload: the spec's
/// `follow.prompt.await_title` while follow-me holds for the operator
/// (`holding-await-operator`), else `None`. The title rule of the shared
/// `followPrompt` (`web-shared/src/hud/follow-prompt.ts`).
pub fn follow_prompt_title(state: &Value) -> Option<String> {
    (state.get("phase").and_then(Value::as_str) == Some("holding-await-operator"))
        .then(|| spec_text("follow.prompt.await_title", &[]))
}

/// The phase of a `follow.state` payload. Follow-me 0.4.9 and older report
/// `coasting` (flying on the prediction of a lost lock) and `lost` (holding):
/// they read as `predicting` and `holding`, the rule of the shared
/// `parseFollowState` (`web-shared/src/hud/nav-types.ts`).
fn follow_phase(state: &Value) -> Option<String> {
    s(state, "phase").map(|p| match p.as_str() {
        "coasting" => "predicting".to_owned(),
        "lost" => "holding".to_owned(),
        _ => p,
    })
}

/// The navigation source (Navigation `plugin.navigation.state`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NavSource {
    pub mode: Option<String>,
    pub active_set: Option<i64>,
    pub extnav_health: Option<String>,
    /// Why the pose is degraded or lost (`health_reason`), when named.
    pub health_reason: Option<String>,
    pub odom_rate_hz: Option<f64>,
    pub obstacle_rate_hz: Option<f64>,
    pub min_obstacle_m: Option<f64>,
    pub gps_fix: Option<i64>,
    /// How old Navigation's pose was when it reported (ms).
    pub pose_age_ms: Option<f64>,
    pub stale: bool,
}

/// The pilot's `position_unverified`: its reason (`flow_disagrees`,
/// `extnav_lost`, `extnav_degraded`) and Navigation's own reason, if any.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PositionUnverified {
    pub reason: String,
    pub detail: Option<String>,
}

/// The latest lidar scan's nearest return.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Lidar {
    pub min_range_m: Option<f64>,
    pub stale: bool,
}

/// The live mapping session (`map_status`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MapSession {
    pub placement: Option<String>,
    pub sweep_points: Option<i64>,
    pub area_m2: Option<f64>,
    pub voxel_m: Option<f64>,
    pub memory_used_mb: Option<f64>,
    pub memory_budget_mb: Option<f64>,
    pub keyframes_dropped: Option<i64>,
    pub stale: bool,
}

/// The mission progress inside the pilot state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PilotMission {
    pub state: Option<String>,
    pub seq: Option<f64>,
    pub total: Option<f64>,
    /// The mission is on its landing sequence.
    pub landing_seq: bool,
}

/// The pilot's banner: a spec text key and its args (values as text), shown
/// until `until_ms` (wall-clock epoch ms) when the pilot gives one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Banner {
    pub text_key: String,
    pub args: Vec<(String, String)>,
    pub until_ms: Option<u64>,
}

/// A mission or return that stopped making progress (`mission_stall`): the
/// vehicle holds and the operator chooses.
#[derive(Debug, Clone, PartialEq)]
pub struct MissionStall {
    /// `mission` or `return`.
    pub kind: String,
    pub seq: Option<f64>,
    pub wp_label: Option<String>,
    pub since_ms: Option<u64>,
    /// Seconds to the automatic action (skip, or land for a return); `None`
    /// holds for the operator.
    pub skip_in_s: Option<f64>,
    /// Seconds until the vehicle flies SmartRTL home on its own: a mission
    /// held for the operator in a confined place (no skip left) that no
    /// operator has answered; `None` otherwise.
    pub return_in_s: Option<f64>,
    pub window_s: Option<f64>,
}

/// Who flies the vehicle and how (the `pilot` kind). Enumerated fields hold
/// only the values the shared parser accepts; anything else reads as absent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pilot {
    /// One of [`PILOT_OWNERS`]; `None` for an owner the chip does not know.
    pub owner: Option<String>,
    /// The owning skill's label, when a skill owns.
    pub skill_label: Option<String>,
    /// The owning skill's plugin and skill ids, for its echoed parameters.
    pub skill_ref: Option<(String, String)>,
    pub control_mode: Option<String>,
    pub profile: Option<String>,
    /// "SIM ×N" while the pilot follows a simulator's clock; `None` on wall
    /// time.
    pub sim_clock: Option<String>,
    pub avoid: Option<String>,
    pub avoid_state: Option<String>,
    pub hold_reason: Option<String>,
    pub min_clearance_m: Option<f64>,
    pub fc_mode: Option<f64>,
    pub fc_mode_name: Option<String>,
    pub vehicle_class: Option<String>,
    pub vtol_transitioning: bool,
    /// The goal, NED metres about the local origin.
    pub goal_ned: Option<[f64; 3]>,
    pub readiness: Vec<ReadinessItem>,
    /// Seconds left before the link-loss action, while the link is lost.
    pub link_loss_in_s: Option<f64>,
    pub link_loss_action: Option<String>,
    /// The link-loss window, seconds; `None` from an agent that omits it.
    pub link_loss_s: Option<f64>,
    pub mission: Option<PilotMission>,
    /// The pilot's banner, while it sends one.
    pub banner: Option<Banner>,
    /// Seconds left before the low-battery return, while it counts down.
    pub rth_countdown_s: Option<f64>,
    /// A mission or return stalled for the operator, while it lasts.
    pub mission_stall: Option<MissionStall>,
    /// Height above ground: a fresh downward rangefinder, else the EKF
    /// height above home (`agl_source`).
    pub agl_m: Option<f64>,
    pub agl_source: Option<String>,
    /// The flight controller's flow and range and the lidar (`sensors`);
    /// `None` without an FC link.
    pub sensors: Option<SensorHealth>,
    /// FC STATUSTEXT at WARNING or worse from the last 30 s, newest first.
    pub statustext: Vec<FcStatusText>,
    /// The setup step a tuning flight is on.
    pub setup_step: Option<String>,
    /// The step of a running launch sequence: a known step other than
    /// `done`, with no error.
    pub launch_step: Option<String>,
    /// While that launch is a supervised lift-off: what it is doing, in the
    /// pilot's words ("Levelling on the ground"). A stop's reason reads on
    /// the launch-failed banner.
    pub liftoff_text: Option<String>,
    /// The FC flies an RTL or land of its own (a failsafe, not an operator's
    /// mode change); the agent omits `fc_failsafe` while it is false.
    pub fc_failsafe: bool,
    /// What tripped that failsafe (`fc`, `rc_or_failsafe`), when the pilot said.
    pub failsafe_cause: Option<String>,
    /// The setup-strings key of the air wizard's last abort that was not the
    /// operator's (`setup.blocker.aborted_<reason>`), while the agent shows it.
    pub setup_abort: Option<String>,
    /// The pilot's own low-battery action once it fired (`low_battery_action`
    /// with no countdown running): `land` or `rth`.
    pub low_battery_action: Option<String>,
    /// While the low-battery countdown runs, whether it ends in a landing
    /// (`low_battery_action` `land`; a GNSS-free copter lands, never returns).
    pub low_battery_countdown_land: bool,
    /// The charge level a low-battery return home needs (`battery_reserve`
    /// `need_pct`) while the estimate of the way home sets it (`basis`
    /// `energy`); `None` on the fixed floor or the ground.
    pub energy_need_pct: Option<u32>,
    /// The horizontal speed cap (m/s) the pilot applies to every setpoint it
    /// flies (`speed_cap.v_h_mps`, e.g. while GNSS-denied); `None` without one.
    pub speed_cap_mps: Option<f64>,
    /// The pilot does not trust the horizontal position; `None` while it
    /// does (or an older agent says nothing).
    pub position_unverified: Option<PositionUnverified>,
    /// Leaves or a ceiling overhead along the lease goal's way, while the
    /// pilot reports both the fly-under height and the distance to it.
    pub overhead_ahead: Option<ados_protocol::pilot::OverheadAhead>,
    /// What the pilot sees obstacles with and the speed that stops inside
    /// its seen-clear range; `None` from an older agent.
    pub sensing: Option<ados_protocol::pilot::Sensing>,
    /// Why the cap (`gnss-denied`, `confined-near`), while one applies.
    pub speed_cap_reason: Option<String>,
    /// The operator's place class, `open` or `confined` (absent reads open).
    pub place_class: String,
    /// The drone is declared to fly with no RC transmitter
    /// (`rc_receiver: absent`; omitted or `present` reads false).
    pub rc_absent: bool,
    /// The route check of the loaded mission, while confined.
    pub route_check: Option<RouteCheck>,
    /// The pilot operations a surface may offer now (`goto`, ...).
    pub surface_ops: Vec<String>,
    /// The arm and landed state; `None` while unknown (the FC is unheard),
    /// which may be airborne, never disarmed.
    pub armed: Option<bool>,
    pub landed: Option<bool>,
    /// Who holds the sticks: the input session's label, else its source.
    pub input_label: Option<String>,
    /// The limits the operator's sticks fly at now; `None` from an older
    /// agent.
    pub profile_limits: Option<ados_protocol::pilot::ProfileLimits>,
    /// The operator's sticks on the latest stick tick, while they own the
    /// vehicle.
    pub stick: Option<ados_protocol::pilot::StickCommand>,
    /// The limits that cut the asked velocity since the previous frame
    /// (`command.limits`, first bound first); empty while it flies none.
    pub command_limits: Vec<String>,
    /// The kind's age when read; `None` when the route did not say.
    pub age_ms: Option<u64>,
    pub stale: bool,
}

/// One `/api/v1/nav/status` reading.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Autonomy {
    pub pilot: Option<Pilot>,
    pub follow: Option<Follow>,
    pub nav: Option<NavSource>,
    pub lidar: Option<Lidar>,
    pub map: Option<MapSession>,
    /// The next waypoint's ETA from a fresh `ar` kind.
    pub ar_eta_s: Option<f64>,
}

impl Autonomy {
    pub fn from_value(v: &Value) -> Self {
        let pilot = kind(v, "pilot").map(|(d, age, stale)| pilot(d, age, stale));
        let follow = kind(v, "follow").map(|(d, age_ms, stale)| Follow {
            phase: follow_phase(d),
            track_id: i(d, "track_id"),
            range_m: f(d, "range_m"),
            range_sigma_m: f(d, "range_sigma_m"),
            reacquire_mode: s(d, "reacquire_mode"),
            reacquire_remaining_s: f(d, "reacquire_remaining_s"),
            search_step: i(d, "search_step"),
            occluder_kind: d
                .get("occluder")
                .and_then(|o| one_of(o, "kind", &["narrow", "wide"])),
            prompt_title: follow_prompt_title(d),
            hold_reason: s(d, "hold_reason"),
            agl_m: f(d, "agl_m"),
            agl_source: one_of(d, "agl_source", &["rng", "baro"]),
            height_lowered_m: f(d, "height_setpoint_m")
                .filter(|_| d.get("height_lowered").and_then(Value::as_bool) == Some(true)),
            height_reason: one_of(d, "height_reason", &["canopy", "canopy_floor"]),
            age_ms,
            stale,
        });
        let nav = kind(v, "nav").map(|(d, _, stale)| NavSource {
            mode: s(d, "mode"),
            active_set: i(d, "active_set"),
            extnav_health: s(d, "extnav_health"),
            health_reason: s(d, "health_reason"),
            odom_rate_hz: f(d, "odom_rate_hz"),
            obstacle_rate_hz: f(d, "obstacle_rate_hz"),
            min_obstacle_m: f(d, "min_obstacle_m"),
            gps_fix: i(d, "gps_fix"),
            pose_age_ms: f(d, "pose_age_ms"),
            stale,
        });
        let lidar = kind(v, "scan").map(|(d, _, stale)| Lidar {
            // `0.0` is "no return" on the scan topic, so it is not a range.
            min_range_m: d
                .get("ranges_m")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_f64)
                .filter(|r| r.is_finite() && *r > 0.0)
                .reduce(f64::min),
            stale,
        });
        let map = kind(v, "map").map(|(d, _, stale)| MapSession {
            placement: s(d, "placement"),
            sweep_points: i(d, "sweep_points"),
            area_m2: f(d, "area_m2"),
            voxel_m: f(d, "voxel_m"),
            memory_used_mb: f(d, "memory_used_mb"),
            memory_budget_mb: f(d, "memory_budget_mb"),
            keyframes_dropped: i(d, "keyframes_dropped"),
            stale,
        });
        let ar_eta_s = kind(v, "ar").and_then(|(d, age, _)| next_waypoint_eta_s(d, age));
        Self {
            pilot,
            follow,
            nav,
            lidar,
            map,
            ar_eta_s,
        }
    }

    /// Whether any kind has ever been published.
    pub fn any(&self) -> bool {
        self.pilot.is_some()
            || self.follow.is_some()
            || self.nav.is_some()
            || self.lidar.is_some()
            || self.map.is_some()
    }

    /// Mark every kind stale: the last good reading stays on screen after a
    /// failed read, but no row of it reads as current.
    pub fn mark_stale(&mut self) {
        if let Some(p) = &mut self.pilot {
            p.stale = true;
        }
        if let Some(f) = &mut self.follow {
            f.stale = true;
        }
        if let Some(n) = &mut self.nav {
            n.stale = true;
        }
        if let Some(l) = &mut self.lidar {
            l.stale = true;
        }
        if let Some(m) = &mut self.map {
            m.stale = true;
        }
    }
}

/// One FC STATUSTEXT line from the pilot kind; `severity` is MAVLink's
/// (0 emergency … 4 warning).
#[derive(Debug, Clone, PartialEq)]
pub struct FcStatusText {
    pub severity: f64,
    pub text: String,
}

/// One pre-flight readiness item from the pilot kind.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadinessItem {
    pub id: String,
    pub ok: bool,
    /// A failing blocking item refuses take-off; otherwise it only warns.
    pub block: bool,
    pub msg: String,
}

#[cfg(test)]
mod tests;
