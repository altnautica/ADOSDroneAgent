use ados_protocol::mavlink::ardupilotmega::*;
use ados_protocol::pilot::{
    ActionReq, ActionResult, Goal, GoalKind, GoalStatus, Holder, Input, Request, VantageSpec,
};

use crate::config::Settings;
use crate::core::{Out, Pilot, Reply};

pub const FC_SYS: u8 = 1;
pub const FC_COMP: u8 = 1;

/// A copter in SITL terms: heartbeat, position, attitude, rangefinder, and
/// a planar lidar in open space.
pub struct World {
    pub p: Pilot,
    pub now: u64,
    pub mode: u32,
    pub armed: bool,
    pub pos: [f32; 3],
    pub vel: [f32; 3],
    pub yaw: f32,
    pub roll: f32,
    pub pitch: f32,
    pub agl: Option<f32>,
    /// GPS_RAW_INT `fix_type` the scripted FC reports (`None`: no GPS
    /// message). A 3-D fix by default, so flight is not speed-capped.
    pub gps_fix: Option<u8>,
    pub landed: bool,
    pub sent: Vec<MavMessage>,
    pub revoked: Vec<(u64, String)>,
    /// HEARTBEAT `type` the scripted FC reports.
    pub mavtype: MavType,
    /// Modes the scripted FC refuses to arm in.
    pub unarmable: Vec<u32>,
    /// The scripted FC refuses DO_SET_MODE to this mode with this result,
    /// sending this STATUSTEXT after its ack (`None`: no text).
    pub refuse_mode: Option<(u32, MavResult, Option<&'static str>)>,
    /// The scripted FC answers DO_SET_MODE with a COMMAND_ACK (off: the ack
    /// is lost).
    pub acks: bool,
    /// Frames the scripted FC sends ahead of its next state.
    pub inbox: Vec<MavMessage>,
    /// Answers to deferred actions, by token.
    pub answers: Vec<(u64, ActionResult)>,
    /// The lidar scans: whenever the pilot's scan is 100 ms old (a test feeds
    /// none of its own), a scan of `scan_ranges` arrives. Off: no scan
    /// arrives at all.
    pub lidar: bool,
    /// The ranges of the scan `lidar` sends, 360 one-degree body bins;
    /// all 0 (no returns: open space) unless a test sets them.
    pub scan_ranges: Vec<f32>,
    /// `goal.status` messages, in order.
    pub goal_status: Vec<GoalStatus>,
    /// The scripted FC sends LOCAL_POSITION_NED (off: its local position
    /// goes stale).
    pub local: bool,
    /// The scripted FC stamps LOCAL_POSITION_NED with its boot time (the
    /// test clock), so the pilot judges position steps
    /// ([`crate::fc::pos_reset`]). Off: 0, never judged, as tests that put
    /// the vehicle somewhere else between ticks need.
    pub fc_clock: bool,
    /// The vehicle flies each tick's setpoint as a stiff FC would: its
    /// horizontal velocity becomes the setpoint's velocity plus the
    /// position loop's pull (gain 1/s) toward the setpoint's position, and
    /// its position moves on it. Off: the vehicle stays where a test puts it.
    pub fly: bool,
    /// The `seq` of the next stick frame [`World::input`] sends.
    pub seq: u32,
    /// The airframe a supervised lift-off flies (balanced unless a test
    /// sets one), and whether it drives the world (from the first attitude
    /// target until the launch leaves its take-off step).
    pub af: crate::liftoff::sim::Airframe,
    pub af_engaged: bool,
    /// The scripted FC echoes PARAM_SET (off: the writes are lost).
    pub answers_params: bool,
    /// The scripted FC streams PID_TUNING and SERVO_OUTPUT_RAW while armed.
    pub rates_stream: bool,
    /// The FC's own parameter table, answered through ArduPilot's request
    /// queue: at most [`FC_PARAM_QUEUE`] by-name reads per heartbeat, the
    /// rest dropped. `None`: the pilot's `p.fc.params` is the table and every
    /// read is answered.
    pub fc_table: Option<std::collections::HashMap<String, f32>>,
    /// Names the FC table holds but whose replies wait in its full reply
    /// queue (never sent during the test).
    pub slow_params: Vec<&'static str>,
    reads_this_beat: usize,
}

/// ArduPilot's by-name read queue (`GCS_Param.cpp`, `param_requests(20)`).
pub const FC_PARAM_QUEUE: usize = 20;

/// A scan with one return, `range_m` away in body bin `bin` (degrees
/// clockwise from the nose): something small, a pole or a person.
pub fn one_return(bin: usize, range_m: f32) -> Vec<f32> {
    let mut r = vec![0.0; 360];
    r[bin] = range_m;
    r
}

/// A scan of a wall `range_m` away across body bins `bin` ± 10 (at 8 m,
/// 2.8 m wide).
pub fn wall(bin: usize, range_m: f32) -> Vec<f32> {
    let mut r = vec![0.0; 360];
    for d in 0..=20 {
        r[(bin + 360 + d - 10) % 360] = range_m;
    }
    r
}

/// A scan of a wall `range_m` straight behind: a surface the lidar sees,
/// out of the way of flight ahead.
pub fn surface_behind(range_m: f32) -> Vec<f32> {
    wall(180, range_m)
}

impl World {
    pub fn new() -> Self {
        let mut p = Pilot::new(Settings::default());
        for (n, v) in [
            ("MAV_GCS_SYSID", 1.0),
            ("FS_GCS_ENABLE", 4.0),
            ("FS_GCS_TIMEOUT", 3.0),
            ("FS_THR_ENABLE", 5.0),
            ("GUID_TIMEOUT", 1.0),
            ("RC_OVERRIDE_TIME", 1.0),
            ("BATT_FS_LOW_ACT", 2.0),
            ("BATT_FS_CRT_ACT", 1.0),
            ("OA_TYPE", 0.0),
            ("AVOID_ENABLE", 2.0),
            ("AVOID_MARGIN", 1.0),
            ("AVOID_BEHAVE", 1.0),
            ("PRX1_TYPE", 2.0),
            ("SRTL_POINTS", 500.0),
            ("SRTL_ACCURACY", 2.0),
            // ArduPilot defaults: the supervised lift-off sets its bits.
            ("GUID_OPTIONS", 0.0),
            ("GCS_PID_MASK", 0.0),
            ("MOT_THST_HOVER", 0.5),
            ("FRAME_CLASS", 1.0),
        ] {
            p.fc.params.insert(n.into(), v);
        }
        let mut w = Self {
            p,
            now: 1000,
            mode: crate::modes::copter::LOITER,
            armed: false,
            pos: [0.0, 0.0, 0.0],
            vel: [0.0; 3],
            yaw: 0.0,
            roll: 0.0,
            pitch: 0.0,
            agl: Some(0.1),
            landed: true,
            sent: Vec::new(),
            gps_fix: Some(3),
            revoked: Vec::new(),
            mavtype: MavType::MAV_TYPE_QUADROTOR,
            unarmable: Vec::new(),
            refuse_mode: None,
            acks: false,
            inbox: Vec::new(),
            answers: Vec::new(),
            lidar: true,
            scan_ranges: vec![0.0; 360],
            goal_status: Vec::new(),
            local: true,
            fc_clock: false,
            fly: false,
            seq: 1,
            af: crate::liftoff::sim::Airframe::balanced(),
            af_engaged: false,
            answers_params: true,
            rates_stream: true,
            fc_table: None,
            slow_params: Vec::new(),
            reads_this_beat: 0,
        };
        w.feed();
        // The pilot has read the FC's table before a test starts: answer
        // each burst of reads, at this instant, until none is left.
        for _ in 0..32 {
            w.collect();
            if w.inbox.is_empty() {
                break;
            }
            w.feed();
        }
        w
    }

    /// A world whose vehicle has no lidar fitted: no scan has arrived and
    /// none will unless the test feeds its own.
    pub fn without_lidar() -> Self {
        let mut w = Self::new();
        w.lidar = false;
        w.p.obstacles = crate::obstacles::Obstacles::new();
        w.p.lidar = crate::sensor_health::LidarTrack::default();
        w
    }

    pub fn heartbeat(&self) -> MavMessage {
        let mut base = MavModeFlag::MAV_MODE_FLAG_CUSTOM_MODE_ENABLED;
        if self.armed {
            base |= MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED;
        }
        MavMessage::HEARTBEAT(HEARTBEAT_DATA {
            custom_mode: self.mode,
            mavtype: self.mavtype,
            autopilot: MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
            base_mode: base,
            system_status: MavState::MAV_STATE_ACTIVE,
            mavlink_version: 3,
        })
    }

    /// Send the FC's current state to the pilot.
    pub fn feed(&mut self) {
        for m in std::mem::take(&mut self.inbox) {
            self.p.on_fc(FC_SYS, FC_COMP, &m, self.now);
        }
        let mut msgs = vec![self.heartbeat()];
        if self.local {
            msgs.push(MavMessage::LOCAL_POSITION_NED(LOCAL_POSITION_NED_DATA {
                time_boot_ms: if self.fc_clock { self.now as u32 } else { 0 },
                x: self.pos[0],
                y: self.pos[1],
                z: self.pos[2],
                vx: self.vel[0],
                vy: self.vel[1],
                vz: self.vel[2],
            }));
        }
        msgs.extend([
            MavMessage::ATTITUDE(ATTITUDE_DATA {
                time_boot_ms: 0,
                roll: self.roll,
                pitch: self.pitch,
                yaw: self.yaw,
                rollspeed: 0.0,
                pitchspeed: 0.0,
                yawspeed: 0.0,
            }),
            MavMessage::EXTENDED_SYS_STATE(EXTENDED_SYS_STATE_DATA {
                vtol_state: MavVtolState::MAV_VTOL_STATE_UNDEFINED,
                landed_state: if self.landed {
                    MavLandedState::MAV_LANDED_STATE_ON_GROUND
                } else {
                    MavLandedState::MAV_LANDED_STATE_IN_AIR
                },
            }),
        ]);
        for m in msgs {
            self.p.on_fc(FC_SYS, FC_COMP, &m, self.now);
        }
        if let Some(a) = self.agl {
            let m = MavMessage::DISTANCE_SENSOR(DISTANCE_SENSOR_DATA {
                time_boot_ms: 0,
                min_distance: 5,
                max_distance: 1200,
                current_distance: (a * 100.0) as u16,
                mavtype: MavDistanceSensor::MAV_DISTANCE_SENSOR_LASER,
                id: 0,
                orientation: MavSensorOrientation::MAV_SENSOR_ROTATION_PITCH_270,
                covariance: 0,
            });
            self.p.on_fc(FC_SYS, FC_COMP, &m, self.now);
        }
        self.feed_airframe();
        if let Some(fix) = self.gps_fix {
            let fix_type = match fix {
                0 => GpsFixType::GPS_FIX_TYPE_NO_GPS,
                1 => GpsFixType::GPS_FIX_TYPE_NO_FIX,
                2 => GpsFixType::GPS_FIX_TYPE_2D_FIX,
                _ => GpsFixType::GPS_FIX_TYPE_3D_FIX,
            };
            let m = MavMessage::GPS_RAW_INT(GPS_RAW_INT_DATA {
                fix_type,
                ..Default::default()
            });
            self.p.on_fc(FC_SYS, FC_COMP, &m, self.now);
        }
        let due = self
            .p
            .obstacles
            .scan_age_ms(self.now)
            .is_none_or(|a| a >= 100);
        if self.lidar && due {
            let scan = serde_json::json!({"frame": "body_frd", "angle_start_rad": 0.0,
                "angle_increment_rad": std::f32::consts::TAU / 360.0, "ranges_m": self.scan_ranges});
            self.p.on_feed(crate::events::TOPIC_SCAN, &scan, self.now);
        }
    }

    /// Advance time by `ms` in 20 ms ticks, feeding state each tick. As the
    /// runtime does, the pilot frame is published every 100 ms of the clock.
    pub fn run(&mut self, ms: u64) {
        let end = self.now + ms;
        while self.now < end {
            self.now += 20;
            self.feed();
            self.p.tick(self.now);
            let from = self.sent.len();
            self.collect();
            if self.fly {
                self.fly_tick(from, 0.02);
            }
            if self.now.is_multiple_of(100) {
                self.p.state_published();
            }
            self.step_airframe(20);
        }
    }

    /// [`World::fly`]: fly the last setpoint sent since `from` for `dt`.
    fn fly_tick(&mut self, from: usize, dt: f32) {
        let Some(sp) = self.sent[from..].iter().rev().find_map(|m| match m {
            MavMessage::SET_POSITION_TARGET_LOCAL_NED(s) => Some(s.clone()),
            _ => None,
        }) else {
            return;
        };
        let mask = sp.type_mask.bits();
        let mut v = [sp.vx, sp.vy];
        if mask & 0b111 == 0 {
            v[0] += sp.x - self.pos[0];
            v[1] += sp.y - self.pos[1];
        }
        self.vel = [v[0], v[1], 0.0];
        self.pos[0] += v[0] * dt;
        self.pos[1] += v[1] * dt;
    }

    pub fn collect(&mut self) {
        // The FC's request queue drains between heartbeats.
        self.reads_this_beat = 0;
        for o in self.p.drain() {
            match o {
                Out::Mav(m) => {
                    self.apply(&m);
                    self.sent.push(*m);
                }
                Out::Revoked(r) => self.revoked.push((r.lease_id, r.reason)),
                Out::Answer(t, a) => self.answers.push((t, a)),
                Out::GoalStatus(s) => self.goal_status.push(s),
                Out::Sticks(_) | Out::Persist { .. } => {}
            }
        }
    }

    /// The scripted FC obeys mode, arm and takeoff commands. Its parameter
    /// table is [`World::fc_table`], else the pilot's own (tests write
    /// `p.fc.params`): a read of a name not in it is answered `PARAM_ERROR`
    /// DOES_NOT_EXIST, as ArduPilot 4.7 answers it.
    fn apply(&mut self, m: &MavMessage) {
        self.apply_airframe(m);
        if let MavMessage::PARAM_REQUEST_READ(r) = m {
            let name = r.param_id.to_str().unwrap_or("").trim_end_matches('\0');
            let value = match &self.fc_table {
                Some(t) => {
                    self.reads_this_beat += 1;
                    if self.reads_this_beat > FC_PARAM_QUEUE || self.slow_params.contains(&name) {
                        return;
                    }
                    t.get(name).copied()
                }
                None => self.p.fc.params.get(name).copied(),
            };
            match value {
                Some(v) if self.fc_table.is_some() => {
                    self.inbox.push(MavMessage::PARAM_VALUE(PARAM_VALUE_DATA {
                        param_value: v,
                        param_count: 0,
                        param_index: 0,
                        param_id: r.param_id,
                        param_type: MavParamType::MAV_PARAM_TYPE_REAL32,
                    }))
                }
                Some(_) => {}
                None => self.inbox.push(MavMessage::PARAM_ERROR(PARAM_ERROR_DATA {
                    param_index: -1,
                    target_system: 255,
                    target_component: 0,
                    param_id: r.param_id,
                    error: MavParamError::MAV_PARAM_ERROR_DOES_NOT_EXIST,
                })),
            }
        }
        if let MavMessage::COMMAND_LONG(c) = m {
            match c.command {
                MavCmd::MAV_CMD_DO_SET_MODE => self.set_mode(c.param2 as u32),
                MavCmd::MAV_CMD_COMPONENT_ARM_DISARM => {
                    if c.param1 < 0.5 || !self.unarmable.contains(&self.mode) {
                        self.armed = c.param1 > 0.5;
                    }
                }
                MavCmd::MAV_CMD_NAV_TAKEOFF => {
                    self.pos[2] = -c.param7;
                    self.agl = Some(c.param7);
                    self.landed = false;
                }
                _ => {}
            }
        }
    }

    /// The scripted FC's answer to DO_SET_MODE.
    fn set_mode(&mut self, mode: u32) {
        let (result, text) = match self.refuse_mode {
            Some((m, result, text)) if m == mode => (result, text),
            _ => {
                self.mode = mode;
                (MavResult::MAV_RESULT_ACCEPTED, None)
            }
        };
        if self.acks {
            self.inbox.push(MavMessage::COMMAND_ACK(COMMAND_ACK_DATA {
                command: MavCmd::MAV_CMD_DO_SET_MODE,
                result,
            }));
        }
        if let Some(t) = text {
            self.inbox.push(MavMessage::STATUSTEXT(STATUSTEXT_DATA {
                severity: MavSeverity::MAV_SEVERITY_WARNING,
                text: t.into(),
            }));
        }
    }

    /// Put the vehicle airborne, armed, in `mode`.
    pub fn airborne(&mut self, mode: u32) {
        self.airborne_at(mode, [0.0, 0.0, -5.0]);
    }

    /// [`World::airborne`] at `pos` (NED, the ground at 0 m). The pilot's
    /// first tick there starts its reference at the vehicle, so a test that
    /// wants the vehicle elsewhere puts it there before, never under a
    /// reference already flying.
    pub fn airborne_at(&mut self, mode: u32, pos: [f32; 3]) {
        self.armed = true;
        self.landed = false;
        self.mode = mode;
        self.pos = pos;
        self.agl = Some(-pos[2]);
        self.feed();
        self.p.tick(self.now);
        self.collect();
    }

    /// A request answered at once; a deferred one runs the world (20 ms
    /// ticks, at most 3 s) until its answer comes.
    pub fn req(&mut self, r: Request) -> Reply {
        let rep = self.req_now(r);
        let Reply::Deferred(token) = rep else {
            return rep;
        };
        let end = self.now + 3000;
        while self.now < end {
            if let Some(i) = self.answers.iter().position(|(t, _)| *t == token) {
                return Reply::Action(self.answers.remove(i).1);
            }
            self.run(20);
        }
        panic!("deferred request {token} never answered");
    }

    /// A request as the pilot replies to it, deferred or not.
    pub fn req_now(&mut self, r: Request) -> Reply {
        let rep = self.p.on_request(r, self.now);
        self.collect();
        rep
    }

    pub fn action(&mut self, name: &str) -> Reply {
        self.req(Request::Action(ActionReq {
            name: name.into(),
            src: "mc".into(),
            args: None,
            id: None,
        }))
    }

    pub fn action_args(&mut self, name: &str, args: serde_json::Value) -> Reply {
        self.req(Request::Action(ActionReq {
            name: name.into(),
            src: "mc".into(),
            args: Some(args),
            id: None,
        }))
    }

    /// The router's state snapshot carrying mission state `state` at `seq`,
    /// through the same path the state socket reader takes.
    pub fn mission(&mut self, state: &str, seq: u32, total: u32) {
        self.mission_from("mission_state", state, seq, total);
    }

    /// [`Self::mission`] with the router's `source`: `mission_state`, or
    /// `fallback` when the FC sends no MISSION_CURRENT mission state.
    pub fn mission_from(&mut self, source: &str, state: &str, seq: u32, total: u32) {
        let snapshot = serde_json::json!({
            "armed": self.armed,
            "mission": {"state": state, "seq": seq, "total": total, "item_reached": seq.checked_sub(1),
                        "id": null, "source": source, "age_ms": 100, "landing_seq": false,
                        "rev": 1, "uploads_seen": 0, "items": [{"seq": 1, "cmd": 16}]},
        });
        let m = crate::router_state::mission_of(snapshot);
        self.p
            .on_feed(crate::router_state::MISSION_TOPIC, &m, self.now);
        self.collect();
    }

    /// COMMAND_LONGs sent with `command` (and, when given, `param1`/`param2`).
    pub fn commands(&self, command: MavCmd, param: Option<(usize, f32)>) -> usize {
        self.sent
            .iter()
            .filter(|m| match m {
                MavMessage::COMMAND_LONG(c) if c.command == command => {
                    param.is_none_or(|(i, v)| [c.param1, c.param2][i] == v)
                }
                _ => false,
            })
            .count()
    }

    /// DO_SET_MODE commands sent for `mode`.
    pub fn set_modes(&self, mode: u32) -> usize {
        self.commands(MavCmd::MAV_CMD_DO_SET_MODE, Some((1, mode as f32)))
    }

    pub fn input(&mut self, axes: [f32; 4]) {
        let r = Request::Input(Input {
            src: "mc".into(),
            session: 1,
            seq: self.seq,
            t_ms: self.now,
            axes,
            buttons: 0,
        });
        self.seq = self.seq.wrapping_add(1);
        self.req(r);
    }

    /// Stream `axes` for `ms`.
    pub fn stick(&mut self, axes: [f32; 4], ms: u64) {
        let end = self.now + ms;
        while self.now < end {
            self.input(axes);
            self.run(20);
        }
    }

    pub fn lease(&mut self) -> u64 {
        let r = self.req(Request::LeaseAcquire {
            holder: Holder {
                plugin_id: "com.example.follow".into(),
                skill_id: "follow-me".into(),
            },
            kind: "motion".into(),
        });
        match r {
            Reply::Lease(l) => l.lease_id.expect("lease granted"),
            other => panic!("{other:?}"),
        }
    }

    pub fn goal(&mut self, lease_id: u64, n: f32, e: f32) {
        let goal = Goal::new(GoalKind::Position, [n, e, -5.0], 5.0);
        self.req(Request::Goal { lease_id, goal });
    }

    /// A position goal to `[n, e, d]` (the harness's 5 m/s).
    pub fn goal_d(&mut self, lease_id: u64, n: f32, e: f32, d: f32) {
        let goal = Goal::new(GoalKind::Position, [n, e, d], 5.0);
        self.req(Request::Goal { lease_id, goal });
    }

    /// A vantage goal at 5 m height, 2 m/s: subject predicted at `pred`,
    /// the search bounded 12 m around `anchor`.
    pub fn vantage_goal(&mut self, lease_id: u64, pred: [f32; 2], anchor: [f32; 2]) {
        let mut goal = Goal::new(GoalKind::Vantage, [0.0, 0.0, -5.0], 2.0);
        goal.vantage = Some(VantageSpec::new(
            pred,
            [0.5, 0.0, 0.5],
            1.0,
            5.0,
            12.0,
            anchor,
        ));
        self.req(Request::Goal { lease_id, goal });
    }

    /// Keep the vantage goal fresh (the lease drops a goal older than 1 s)
    /// for `ms`.
    pub fn hold_vantage(&mut self, lease_id: u64, pred: [f32; 2], anchor: [f32; 2], ms: u64) {
        let end = self.now + ms;
        while self.now < end {
            self.vantage_goal(lease_id, pred, anchor);
            self.run(200);
        }
    }

    pub fn last_setpoint(&self) -> Option<&SET_POSITION_TARGET_LOCAL_NED_DATA> {
        self.sent.iter().rev().find_map(|m| match m {
            MavMessage::SET_POSITION_TARGET_LOCAL_NED(s) => Some(s),
            _ => None,
        })
    }
}
