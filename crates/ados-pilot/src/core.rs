//! The pilot state machine: FC frames, client requests and a 50 Hz tick in;
//! MAVLink frames, lease events and published state out. Pure and
//! deterministic (time is passed in), so every C2/C10 rule is unit-tested
//! without sockets.

mod actions;
mod control;
mod events;
mod nav_cap;
mod operator;
mod position_check;
mod requests;
mod state;
mod take;
mod touchdown;

use std::collections::HashMap;

use ados_protocol::mavlink::ardupilotmega::MavMessage;
use ados_protocol::pilot::{
    ActionResult, Banner, GoalStatus, LeaseReply, LeaseRevoked, Mission, Owner, Sticks, ViewDemand,
};

use crate::authority::Authority;
use crate::config::Settings;
use crate::failsafe::Failsafe;
use crate::fc::{FcState, Tx};
use crate::fixedwing::Fixedwing;
use crate::flows::{HoldDetector, Launch, Sessions};
use crate::geo::V2;
use crate::model::Multirotor;
use crate::modes::Family;
use crate::obstacles::{Obstacles, Scratch};
use crate::planner::Plan;

/// Control loop period.
pub const TICK_MS: u64 = 20;
/// Planner and grid rebuild period (5 Hz).
pub const PLAN_PERIOD_MS: u64 = 200;
/// `sticks` messages to the lease holder (20 Hz).
pub const STICKS_PERIOD_MS: u64 = 50;
/// A mode change within this long of our own DO_SET_MODE is ours.
pub const OWN_MODE_WINDOW_MS: u64 = 3000;
/// A mode change within this long of a ground station's mode command for it
/// (seen by the router) is the operator's, not a failsafe.
pub const CLIENT_MODE_WINDOW_MS: u64 = 2000;

/// Something the runtime must deliver.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// A frame for the flight controller.
    Mav(Box<MavMessage>),
    /// `lease.revoked`, to every subscriber (the plugin host routes it).
    Revoked(LeaseRevoked),
    /// `sticks`, to every subscriber while a skill owns motion.
    Sticks(Sticks),
    /// `goal.status`, at every replan while the lease goal is a vantage
    /// goal (the plugin host forwards it to the holder only).
    GoalStatus(GoalStatus),
    /// Persist `section.key = value` in the agent config.
    Persist {
        section: &'static str,
        key: &'static str,
        value: String,
    },
    /// The answer to a deferred action ([`Reply::Deferred`]).
    Answer(u64, ActionResult),
}

/// The reply to one request on the requester's connection.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    None,
    Lease(LeaseReply),
    Action(ActionResult),
    /// Answered later by [`Out::Answer`] with this token.
    Deferred(u64),
}

/// A swarm velocity goal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SwarmGoal {
    pub vel: [f32; 3],
    pub yaw: Option<f32>,
    pub at_ms: u64,
}

pub struct Pilot {
    pub settings: Settings,
    pub fc: FcState,
    pub auth: Authority,
    pub obstacles: Obstacles,
    /// The scanning lidar's scan arrivals and driver status.
    pub lidar: crate::sensor_health::LidarTrack,
    /// The FC IMU feeding the lidar pose (`map_status.imu_feed`).
    pub nav_imu: crate::nav_imu::NavImuTrack,
    mr: Multirotor,
    fw: Fixedwing,
    /// The brake's verdict this tick (stick flight, or a braked goal).
    brake_avoid: ados_protocol::pilot::AvoidState,
    plan: Plan,
    plan_ms: u64,
    sessions: Sessions,
    failsafe: Failsafe,
    takeover: HoldDetector,
    mission_preempt: HoldDetector,
    land_protect: HoldDetector,
    launch: Option<Launch>,
    banner: Option<Banner>,
    hold_reason: Option<String>,
    goto: Option<[f32; 3]>,
    swarm: Option<SwarmGoal>,
    mission: Option<Mission>,
    mission_paused: bool,
    own_mode: Option<(u32, u64)>,
    /// The last mode command a ground station sent the FC, as the router
    /// reported it.
    client_mode: Option<(events::ClientMode, u64)>,
    landing: bool,
    /// A landing reaching the ground, from the pilot's own evidence
    /// ([`touchdown`]).
    touchdown: touchdown::Touchdown,
    /// Outside `ados`: the hold position already given to the FC. Its
    /// GUIDED keeps it, so it is not streamed again, and a ground station's
    /// reposition replaces it instead of being overwritten on the next tick.
    fc_hold: Option<[f32; 3]>,
    /// The FC switched itself (or an RC switch or failsafe did) to an RTL or
    /// land mode: nothing that would put it in GUIDED or AUTO (Take control,
    /// Pause, a goto, a lease, Resume, a launch) runs until the operator
    /// changes the FC's mode.
    fc_failsafe: bool,
    /// Why the FC flies `fc_failsafe`: the `fc_mode_cause` code at the mode
    /// change. `None` while `fc_failsafe` is false.
    failsafe_cause: Option<&'static str>,
    take: take::Take,
    rth_deadline_ms: Option<u64>,
    /// The proximity brake and no-progress rule over FC-flown missions and
    /// returns, and the return's land fallback.
    sup: control::Supervisor,
    /// The confined-place route check and cap ([`control::Route`]).
    route: control::Route,
    low_batt_cancelled: bool,
    /// The low-battery action counting down or taken (`land` / `rth`),
    /// published as `low_battery_action`.
    low_batt_action: Option<&'static str>,
    view: ViewDemand,
    scratch: HashMap<String, Scratch>,
    obstacles_ms: u64,
    sticks_ms: u64,
    params_ms: u64,
    setpoint: Option<[f32; 3]>,
    plan_hold: Option<[f32; 3]>,
    /// Where a skill goal or goto holds while the obstacle data is stale,
    /// from the tick it went stale; cleared on any tick that does not keep it.
    stale_hold: Option<[f32; 3]>,
    /// The EKF source set the FC flies: the last one vision-nav's `nav.state`
    /// named, the boot set before any ([`nav_cap`]).
    nav_set: u8,
    /// The last capped horizontal velocity flown and its time ([`nav_cap`]).
    cap_vel: Option<(V2, u64)>,
    /// The FC's flow against the EKF's velocity ([`position_check`]).
    flow_check: position_check::FlowCheck,
    /// vision-nav's last `nav.state` ([`position_check`]).
    extnav: Option<position_check::ExtNav>,
    last_tick_ms: Option<u64>,
    controller_lost_shown: bool,
    /// Setup engine verdict: `None` unknown (no item), `Some(None)` complete,
    /// `Some(Some(msg))` incomplete.
    pub setup_needed: Option<Option<String>>,
    /// The setup engine's active step, published while the owner is `Tuning`.
    pub setup_step: Option<&'static str>,
    /// The setup-strings key of the air wizard's last non-operator abort,
    /// published while the setup engine still shows it.
    pub setup_abort: Option<String>,
    /// Wall-clock epoch ms at monotonic 0, for published absolute times.
    pub epoch_offset_ms: u64,
    /// Sticks handed to the setup engine while the owner is `Tuning`.
    pub tuning_axes: [f32; 4],
    out: Vec<Out>,
}

impl Pilot {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            fc: FcState::default(),
            auth: Authority::default(),
            obstacles: Obstacles::new(),
            lidar: Default::default(),
            nav_imu: Default::default(),
            mr: Multirotor::default(),
            fw: Fixedwing::default(),
            plan: Plan::hold(ados_protocol::pilot::AvoidState::Clear),
            plan_ms: 0,
            sessions: Sessions::default(),
            failsafe: Failsafe::default(),
            takeover: HoldDetector::default(),
            brake_avoid: ados_protocol::pilot::AvoidState::Clear,
            mission_preempt: HoldDetector::default(),
            land_protect: HoldDetector::default(),
            launch: None,
            banner: None,
            hold_reason: None,
            goto: None,
            swarm: None,
            mission: None,
            mission_paused: false,
            own_mode: None,
            client_mode: None,
            landing: false,
            touchdown: touchdown::Touchdown::default(),
            fc_hold: None,
            fc_failsafe: false,
            failsafe_cause: None,
            take: take::Take::default(),
            rth_deadline_ms: None,
            sup: control::Supervisor::default(),
            route: control::Route::default(),
            low_batt_cancelled: false,
            low_batt_action: None,
            view: ViewDemand::default(),
            scratch: HashMap::new(),
            obstacles_ms: 0,
            sticks_ms: 0,
            params_ms: 0,
            setpoint: None,
            plan_hold: None,
            stale_hold: None,
            nav_set: nav_cap::BOOT_SOURCE_SET,
            cap_vel: None,
            flow_check: Default::default(),
            extnav: None,
            last_tick_ms: None,
            controller_lost_shown: false,
            setup_needed: None,
            setup_step: None,
            setup_abort: None,
            epoch_offset_ms: 0,
            tuning_axes: [0.0; 4],
            out: Vec::new(),
        }
    }

    /// Take everything produced since the last drain.
    pub fn drain(&mut self) -> Vec<Out> {
        std::mem::take(&mut self.out)
    }

    pub fn owner(&self) -> Owner {
        self.auth.owner()
    }

    fn tx(&self) -> Tx {
        Tx {
            sysid: self.fc.sysid,
            compid: self.fc.compid,
        }
    }

    fn family(&self) -> Family {
        Family::from_class(self.fc.vehicle_class())
    }

    fn mav(&mut self, m: MavMessage) {
        self.out.push(Out::Mav(Box::new(m)));
    }

    /// Command an FC mode. Every pilot-initiated mode command is logged with
    /// the code site that asked for it.
    #[track_caller]
    fn set_mode(&mut self, mode: u32, now_ms: u64) {
        let site = std::panic::Location::caller();
        tracing::info!(
            target: "ados_pilot",
            mode = %self.family().mode_name(mode),
            reason = %format_args!("{}:{}", site.file(), site.line()),
            "pilot_mode_command"
        );
        self.own_mode = Some((mode, now_ms));
        let m = self.tx().set_mode(mode);
        self.mav(m);
    }

    fn set_owner(&mut self, owner: Owner, reason: &str) {
        self.fc_hold = None;
        if let Some(r) = self.auth.set_owner(owner, reason) {
            self.out.push(Out::Revoked(r));
        }
    }

    fn revoke(&mut self, reason: &str) {
        if let Some(r) = self.auth.revoke(reason) {
            self.out.push(Out::Revoked(r));
        }
    }

    fn mode_is_ours(&self, mode: u32, now_ms: u64) -> bool {
        self.own_mode
            .is_some_and(|(m, t)| m == mode && now_ms.saturating_sub(t) < OWN_MODE_WINDOW_MS)
    }

    fn in_guided(&self) -> bool {
        self.fc.custom_mode == self.family().guided()
    }

    fn pos(&self, now_ms: u64) -> Option<[f32; 3]> {
        self.fc.fresh_local(now_ms).map(|p| [p.n, p.e, p.d])
    }

    fn pos_ne(&self, now_ms: u64) -> Option<V2> {
        self.pos(now_ms).map(|p| [p[0], p[1]])
    }

    fn yaw(&self, now_ms: u64) -> f32 {
        self.fc.fresh_attitude(now_ms).map(|a| a.yaw).unwrap_or(0.0)
    }

    /// Height above ground per `pilot.height_ref`.
    pub fn agl(&self, now_ms: u64) -> Option<f32> {
        if self.settings.pilot.height_ref == crate::config::HeightRef::Agl {
            if let Some(a) = self.fc.rangefinder_agl(now_ms) {
                return Some(a);
            }
        }
        self.fc.ekf_height(now_ms)
    }

    /// The obstacle margin flown: a skill goal's own, else the configured
    /// one, either held to the place class's floor.
    fn margin(&self) -> f32 {
        let p = &self.settings.pilot;
        self.auth
            .lease()
            .and_then(|l| l.goal.as_ref())
            .and_then(|g| g.margin_m)
            .map_or_else(|| p.effective_margin_m(), |m| p.floored_margin_m(m))
    }
}

#[cfg(test)]
mod tests;
