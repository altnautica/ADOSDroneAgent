//! Client requests: sticks sessions, actions, leases, gotos and swarm goals.

use ados_protocol::pilot::{ActionResult, Holder, LeaseReply, Owner, Request};

use super::{Pilot, Reply, SwarmGoal};

impl Pilot {
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
                let result = ActionResult {
                    name: a.name,
                    id: a.id,
                    ok: res.is_ok(),
                    err: res.err(),
                };
                self.take
                    .defer(result)
                    .map_or_else(Reply::Action, Reply::Deferred)
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
        // flies on its own; only the operator's mode change (the Mode sheet,
        // Hand to FC) ends it.
        if self.fc_failsafe {
            return refuse("failsafe", None);
        }
        // A skill's goals fly on the estimate.
        if self.position_unverified(now_ms).is_some() {
            return refuse("position_unverified", None);
        }
        if self.touchdown.floor_d.is_some() {
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

    pub(super) fn start_goto(&mut self, target: [f32; 3], now_ms: u64) {
        if !self.fc.armed || self.landed() || self.fc_failsafe {
            return;
        }
        if self.position_unverified(now_ms).is_some() {
            return;
        }
        // A tuning flight keeps the vehicle; with unknown obstacles, or no
        // obstacle sensor while avoid is on, no new goto is flown.
        if matches!(self.owner(), Owner::Tuning) || self.obstacle_data_state(now_ms).is_some() {
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
        if !self.fc.armed || !self.in_guided() || self.obstacle_data_stale(g.at_ms) {
            return;
        }
        if self.position_unverified(g.at_ms).is_some() {
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
    pub(super) fn brake_hold(&mut self, now_ms: u64, reason: Option<&str>) {
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
