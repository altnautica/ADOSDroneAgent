use ados_protocol::mavlink::ardupilotmega::*;
use ados_protocol::pilot::{ActionReq, Goal, GoalKind, Holder, Input, Request};

use crate::config::Settings;
use crate::core::{Out, Pilot, Reply};

pub const FC_SYS: u8 = 1;
pub const FC_COMP: u8 = 1;

/// A copter in SITL terms: heartbeat, position, attitude, rangefinder.
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
    pub landed: bool,
    pub sent: Vec<MavMessage>,
    pub revoked: Vec<(u64, String)>,
    /// HEARTBEAT `type` the scripted FC reports.
    pub mavtype: MavType,
    /// Modes the scripted FC refuses to arm in.
    pub unarmable: Vec<u32>,
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
            revoked: Vec::new(),
            mavtype: MavType::MAV_TYPE_QUADROTOR,
            unarmable: Vec::new(),
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
                Out::Sticks(_) | Out::Persist { .. } => {}
            }
        }
    }

    /// The scripted FC obeys mode, arm and takeoff commands.
    fn apply(&mut self, m: &MavMessage) {
        if let MavMessage::COMMAND_LONG(c) = m {
            match c.command {
                MavCmd::MAV_CMD_DO_SET_MODE => self.mode = c.param2 as u32,
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

    pub fn req(&mut self, r: Request) -> Reply {
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
        let snapshot = serde_json::json!({
            "armed": self.armed,
            "mission": {"state": state, "seq": seq, "total": total, "item_reached": seq.checked_sub(1),
                        "id": null, "source": "mission_state", "age_ms": 100, "landing_seq": false,
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
        let goal = Goal {
            kind: GoalKind::Position,
            n,
            e,
            d: -5.0,
            vn: 0.0,
            ve: 0.0,
            vd: 0.0,
            yaw: None,
            yaw_rate: None,
            subject_ne: None,
            max_speed_mps: 5.0,
            candidates: None,
            margin_m: None,
        };
        self.req(Request::Goal { lease_id, goal });
    }

    pub fn last_setpoint(&self) -> Option<&SET_POSITION_TARGET_LOCAL_NED_DATA> {
        self.sent.iter().rev().find_map(|m| match m {
            MavMessage::SET_POSITION_TARGET_LOCAL_NED(s) => Some(s),
            _ => None,
        })
    }
}
