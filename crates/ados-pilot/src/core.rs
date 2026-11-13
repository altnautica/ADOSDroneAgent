//! The pilot state machine: FC frames, client requests and a 50 Hz tick in;
//! MAVLink frames, lease events and published state out. Pure and
//! deterministic (time is passed in), so every C2/C10 rule is unit-tested
//! without sockets.

mod actions;
mod control;
mod events;
mod operator;
mod state;
mod touchdown;

use std::collections::HashMap;

use ados_protocol::mavlink::ardupilotmega::MavMessage;
use ados_protocol::pilot::{
    ActionResult, Banner, Holder, LeaseReply, LeaseRevoked, Mission, Owner, Request, Sticks,
    ViewDemand, CLIENT_MODE_TOPIC,
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
    /// Persist `section.key = value` in the agent config.
    Persist {
        section: &'static str,
        key: &'static str,
        value: String,
    },
}

/// The reply to one request on the requester's connection.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    None,
    Lease(LeaseReply),
    Action(ActionResult),
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
    /// On the ground after a landing, until a take-off, climb or disarm: the
    /// NED down of the floor ([`touchdown`]).
    touchdown: Option<f32>,
    /// Outside `ados`: the hold position already given to the FC. Its
    /// GUIDED keeps it, so it is not streamed again, and a ground station's
    /// reposition replaces it instead of being overwritten on the next tick.
    fc_hold: Option<[f32; 3]>,
    /// The FC switched itself (or an RC switch or failsafe did) to an RTL or
    /// land mode: a surface goto waits until the operator changes mode.
    fc_failsafe: bool,
    /// Why the FC flies `fc_failsafe`: the `fc_mode_cause` code at the mode
    /// change. `None` while `fc_failsafe` is false.
    failsafe_cause: Option<&'static str>,
    rth_deadline_ms: Option<u64>,
    low_batt_cancelled: bool,
    view: ViewDemand,
    scratch: HashMap<String, Scratch>,
    obstacles_ms: u64,
    sticks_ms: u64,
    params_ms: u64,
    setpoint: Option<[f32; 3]>,
    plan_hold: Option<[f32; 3]>,
    last_tick_ms: Option<u64>,
    controller_lost_shown: bool,
    /// Setup engine verdict: `None` unknown (no item), `Some(None)` complete,
    /// `Some(Some(msg))` incomplete.
    pub setup_needed: Option<Option<String>>,
    /// The setup engine's active step, published while the owner is `Tuning`.
    pub setup_step: Option<&'static str>,
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
            touchdown: None,
            fc_hold: None,
            fc_failsafe: false,
            failsafe_cause: None,
            rth_deadline_ms: None,
            low_batt_cancelled: false,
            view: ViewDemand::default(),
            scratch: HashMap::new(),
            obstacles_ms: 0,
            sticks_ms: 0,
            params_ms: 0,
            setpoint: None,
            plan_hold: None,
            last_tick_ms: None,
            controller_lost_shown: false,
            setup_needed: None,
            setup_step: None,
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

    fn set_mode(&mut self, mode: u32, now_ms: u64) {
        if self.holds_land(mode) {
            return;
        }
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

    fn margin(&self) -> f32 {
        self.auth
            .lease()
            .and_then(|l| l.goal.as_ref())
            .and_then(|g| g.margin_m)
            .unwrap_or(self.settings.pilot.obstacle_margin_m)
    }

    /// Handle one client request.
    pub fn on_request(&mut self, req: Request, now_ms: u64) -> Reply {
        match req {
            Request::Input(i) => {
                self.failsafe.on_input(now_ms);
                self.sessions
                    .offer(&i.src, i.session, i.axes, i.buttons, now_ms);
                self.controller_lost_shown = false;
                Reply::None
            }
            Request::Release { src, session } => {
                self.failsafe.on_surface(now_ms);
                if self.sessions.release(&src, session, now_ms) && self.owner() == Owner::Pilot {
                    self.brake_hold(now_ms, None);
                }
                Reply::None
            }
            Request::SessionTake { src, session } => {
                self.failsafe.on_surface(now_ms);
                self.sessions.take(&src, session);
                Reply::None
            }
            Request::Hb { .. } => {
                self.failsafe.on_surface(now_ms);
                Reply::None
            }
            Request::Action(a) => {
                // Agent-internal sources are not an operator surface: they
                // must not start the operator link-loss timer.
                if !matches!(a.src.as_str(), "plugin-host" | "router" | "setup") {
                    self.failsafe.on_surface(now_ms);
                }
                let res = self.action(&a.name, a.args.as_ref(), now_ms);
                Reply::Action(ActionResult {
                    name: a.name,
                    id: a.id,
                    ok: res.is_ok(),
                    err: res.err(),
                })
            }
            Request::LeaseAcquire { holder, kind } => {
                Reply::Lease(self.lease_acquire(holder, &kind, now_ms))
            }
            Request::LeaseRelease { lease_id } => {
                if self.auth.release(lease_id) {
                    self.brake_hold(now_ms, None);
                    self.skill_ended(now_ms);
                }
                Reply::None
            }
            Request::Goal { lease_id, goal } => {
                self.auth.goal(lease_id, goal, now_ms);
                Reply::None
            }
            Request::Goto { n, e, d, .. } => {
                // The router's redirect of a ground station's reposition: it
                // never pulls the FC out of its failsafe RTL or land (the
                // router answers the sender TEMPORARILY_REJECTED).
                if !self.fc_failsafe {
                    self.start_goto([n, e, d], now_ms);
                }
                Reply::None
            }
            Request::SwarmGoal { vn, ve, vd, yaw } => {
                self.on_swarm(SwarmGoal {
                    vel: [vn, ve, vd],
                    yaw,
                    at_ms: now_ms,
                });
                Reply::None
            }
            Request::ViewDemand(v) => {
                self.view = v;
                Reply::None
            }
            Request::Feed { topic, payload } => {
                let json = serde_json::to_value(&payload).unwrap_or_default();
                self.on_feed(&topic, &json, now_ms);
                Reply::None
            }
            Request::Subscribe | Request::Other { .. } => Reply::None,
        }
    }

    /// Topics from the plugin event export, the router's state socket
    /// (`mission`) or a `feed` relay.
    pub fn on_feed(&mut self, topic: &str, payload: &serde_json::Value, now_ms: u64) {
        match topic {
            "plugin.world-engine.scan" => {
                let Some(pos) = self.pos_ne(now_ms) else {
                    return;
                };
                let att = self.fc.fresh_attitude(now_ms).unwrap_or_default();
                let level = crate::obstacles::Level {
                    roll: att.roll,
                    pitch: att.pitch,
                    yaw: att.yaw,
                    agl: self.agl(now_ms),
                };
                self.obstacles.ingest_scan(payload, level, pos, now_ms);
            }
            "plugin.world-engine.occupancy" => self.obstacles.ingest_occupancy(payload),
            "plugin.world-engine.pose" => {
                let pos = self.pos_ne(now_ms);
                self.obstacles.ingest_pose(payload, pos);
            }
            "mission" => {
                let m = <Mission as serde::Deserialize>::deserialize(payload).ok();
                self.on_mission(m, now_ms);
            }
            CLIENT_MODE_TOPIC => {
                if let Some(c) = events::ClientMode::parse(payload) {
                    self.client_mode = Some((c, now_ms));
                }
            }
            _ => {}
        }
    }

    fn lease_acquire(&mut self, holder: Holder, kind: &str, now_ms: u64) -> LeaseReply {
        let refuse = |err: &str, holder: Option<Holder>| LeaseReply {
            ok: false,
            lease_id: None,
            err: Some(err.to_owned()),
            holder,
        };
        if kind != "motion" {
            return refuse("unknown_kind", None);
        }
        if !self.fc.armed {
            return refuse("disarmed", None);
        }
        let class = self.fc.vehicle_class();
        if class == ados_protocol::pilot::VehicleClass::Fixedwing || self.fc.vtol_forward() {
            return refuse("vehicle_class", None);
        }
        if self.fc.firmware != Some(crate::fc_profile::Firmware::Ardupilot)
            || self.pos(now_ms).is_none()
        {
            return refuse("not_guided_capable", None);
        }
        // A skill or the swarm never pulls the FC out of an RTL or land it
        // flies on its own; the operator's Take control or Mode sheet does.
        if self.fc_failsafe {
            return refuse("failsafe", None);
        }
        if self.touchdown.is_some() {
            return refuse("landed", None);
        }
        if self.owner() == Owner::Tuning {
            return refuse("busy", None);
        }
        match self.auth.grant(holder, now_ms) {
            Ok(id) => {
                if !self.in_guided() {
                    let g = self.family().guided();
                    self.set_mode(g, now_ms);
                }
                self.goto = None;
                self.swarm = None;
                self.plan_ms = 0;
                self.on_skill_granted(now_ms);
                LeaseReply {
                    ok: true,
                    lease_id: Some(id),
                    err: None,
                    holder: None,
                }
            }
            Err((_, h)) => refuse("busy", h),
        }
    }

    fn start_goto(&mut self, target: [f32; 3], now_ms: u64) {
        if !self.fc.armed || self.landed() || self.fc_failsafe {
            return;
        }
        if matches!(self.owner(), Owner::Tuning) {
            return;
        }
        self.goto = Some(target);
        self.set_owner(Owner::Goto, "goto");
        if !self.in_guided() {
            let g = self.family().guided();
            self.set_mode(g, now_ms);
        }
        self.plan_ms = 0;
    }

    fn on_swarm(&mut self, g: SwarmGoal) {
        if !self.fc.armed || !self.in_guided() {
            return;
        }
        match self.owner() {
            Owner::Pilot if self.sessions.effective_axes().iter().all(|a| a.abs() < 0.3) => {
                self.set_owner(Owner::Swarm, "swarm");
            }
            Owner::Swarm => {}
            _ => return,
        }
        self.swarm = Some(g);
    }

    /// Brake to a hold as `pilot`.
    fn brake_hold(&mut self, now_ms: u64, reason: Option<&str>) {
        if matches!(
            self.owner(),
            Owner::Idle | Owner::FcMode | Owner::MissionFc | Owner::Tuning
        ) {
            return;
        }
        self.set_owner(Owner::Pilot, reason.unwrap_or("brake"));
        self.goto = None;
        self.swarm = None;
        let v = self
            .fc
            .fresh_local(now_ms)
            .map(|p| [p.vn, p.ve])
            .unwrap_or([0.0, 0.0]);
        self.mr.engage(v);
        self.hold_reason = reason.map(str::to_owned);
    }
}

#[cfg(test)]
mod tests;
