use ados_protocol::mavlink::ardupilotmega::*;
use ados_protocol::pilot::{ActionReq, ActionResult, Goal, GoalKind, Holder, Input, Request};

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
    /// none of its own), an open-space scan (no returns) arrives. Off: no scan
    /// arrives at all.
    pub lidar: bool,
}

impl World {
    pub fn new() -> Self {
        let mut p = Pilot::new(Settings::default());
        for (n, v) in [
            ("MAV_GCS_SYSID", 1.0),
            ("FS_GCS_ENABLE", 1.0),
            ("FS_GCS_TIMEOUT", 3.0),
            ("GUID_TIMEOUT", 1.0),
            ("RC_OVERRIDE_TIME", 1.0),
            ("BATT_FS_LOW_ACT", 2.0),
            ("BATT_FS_CRT_ACT", 1.0),
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
        };
        w.feed();
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
        let msgs = vec![
            self.heartbeat(),
            MavMessage::LOCAL_POSITION_NED(LOCAL_POSITION_NED_DATA {
                time_boot_ms: 0,
                x: self.pos[0],
                y: self.pos[1],
                z: self.pos[2],
                vx: self.vel[0],
                vy: self.vel[1],
                vz: self.vel[2],
            }),
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
        ];
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
            let open = serde_json::json!({"frame": "body_frd", "angle_start_rad": 0.0,
                "angle_increment_rad": std::f32::consts::TAU / 360.0, "ranges_m": vec![0.0; 360]});
            self.p.on_feed("plugin.world-engine.scan", &open, self.now);
        }
    }

    /// Advance time by `ms` in 20 ms ticks, feeding state each tick.
    pub fn run(&mut self, ms: u64) {
        let end = self.now + ms;
        while self.now < end {
            self.now += 20;
            self.feed();
            self.p.tick(self.now);
            self.collect();
        }
    }

    pub fn collect(&mut self) {
        for o in self.p.drain() {
            match o {
                Out::Mav(m) => {
                    self.apply(&m);
                    self.sent.push(*m);
                }
                Out::Revoked(r) => self.revoked.push((r.lease_id, r.reason)),
                Out::Answer(t, a) => self.answers.push((t, a)),
                Out::Sticks(_) | Out::Persist { .. } => {}
            }
        }
    }

    /// The scripted FC obeys mode, arm and takeoff commands.
    fn apply(&mut self, m: &MavMessage) {
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
        self.armed = true;
        self.landed = false;
        self.mode = mode;
        self.pos = [0.0, 0.0, -5.0];
        self.agl = Some(5.0);
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
            seq: 0,
            t_ms: self.now,
            axes,
            buttons: 0,
        });
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

    pub fn last_setpoint(&self) -> Option<&SET_POSITION_TARGET_LOCAL_NED_DATA> {
        self.sent.iter().rev().find_map(|m| match m {
            MavMessage::SET_POSITION_TARGET_LOCAL_NED(s) => Some(s),
            _ => None,
        })
    }
}
