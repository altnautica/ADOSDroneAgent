//! The simulated ArduPilot copter the pilot flies: a parameter server, the
//! commands the pilot sends (mode, arm, take-off, mission pause and current
//! item), GUIDED setpoints flown through a first-order velocity response
//! and their heading or yaw rate through ArduCopter's yaw limits, AUTO
//! waypoints, SmartRTL and LAND returns that land and disarm, a battery
//! that drains while armed, and the supervised lift-off flown on
//! [`Airframe`]. Wind gusts and sensor noise come from the run's seeded
//! [`Rng`]; nothing reads a clock or a socket.

use std::collections::BTreeMap;

use ados_pilot::liftoff::sim::Airframe;
use ados_pilot::modes::copter;
use ados_protocol::mavlink::ardupilotmega::*;

use crate::rng::Rng;

/// Gravity, m/s².
const G: f32 = 9.81;
/// The velocity loop's time constant, s.
const VEL_TAU_S: f32 = 0.25;
/// The position loop's pull toward a position target, 1/s.
const POS_GAIN: f32 = 1.0;
/// Speed limits of the simulated position controller, m/s: over the
/// benchmark's fastest goal (10 m/s).
const MAX_H_MPS: f32 = 12.0;
const MAX_UP_MPS: f32 = 2.5;
const MAX_DOWN_MPS: f32 = 1.5;
/// GUIDED take-off climb rate, m/s.
const TAKEOFF_MPS: f32 = 1.5;
/// A waypoint counts as reached this close, m.
const WP_RADIUS_M: f32 = 1.0;
/// A GUIDED setpoint older than this is dropped and the copter holds
/// (ArduPilot's `GUID_TIMEOUT` stops a velocity target; position holds).
const SETPOINT_TIMEOUT_MS: u64 = 3_000;
/// LAND, RTL and SmartRTL disarm this long after touchdown.
const LAND_DISARM_MS: u64 = 1_000;
/// The rangefinder sits this far above the ground on the skids, m.
const SKID_M: f32 = 0.1;
/// The heading, as ArduCopter's attitude controller turns it in GUIDED: a
/// heading target is pulled at the yaw angle gain (`ATC_ANG_YAW_P` 4.5) and
/// slewed at no more than `ATC_SLEW_YAW` (6000 cdeg/s); a yaw-rate target
/// is flown up to this rate limit; the rate reached follows the target
/// through the input shaping's time constant (`ATC_INPUT_TC` 0.15 s).
const YAW_P: f32 = 4.5;
const YAW_SLEW_DPS: f32 = 60.0;
const YAW_RATE_MAX_DPS: f32 = 90.0;
const YAW_TC_S: f32 = 0.15;
/// `SET_POSITION_TARGET_LOCAL_NED` type-mask bits: ignore yaw, yaw rate.
const MASK_YAW_IGNORE: u16 = 1024;
const MASK_YAW_RATE_IGNORE: u16 = 2048;

/// Heartbeat and SYS_STATUS period.
pub const HEARTBEAT_MS: u64 = 500;
/// EXTENDED_SYS_STATE and GPS_RAW_INT period.
const STATUS_MS: u64 = 100;

/// The FC's parameter table: what the pilot reads to pass readiness on a
/// GNSS copter with a SmartRTL low-battery action and a Land critical one.
pub fn params() -> BTreeMap<String, f32> {
    [
        ("MAV_GCS_SYSID", 1.0),
        ("FS_GCS_ENABLE", 4.0),
        ("FS_GCS_TIMEOUT", 3.0),
        ("FS_THR_ENABLE", 5.0),
        ("GUID_TIMEOUT", 1.0),
        ("RC_OVERRIDE_TIME", 1.0),
        ("OA_TYPE", 0.0),
        ("AVOID_ENABLE", 0.0),
        ("SRTL_POINTS", 500.0),
        ("SRTL_ACCURACY", 2.0),
        ("GUID_OPTIONS", 0.0),
        ("GCS_PID_MASK", 0.0),
        ("MOT_THST_HOVER", 0.5),
        ("MOT_PWM_MIN", 1000.0),
        ("MOT_PWM_MAX", 2000.0),
        ("FRAME_CLASS", 1.0),
        ("BATT_MONITOR", 4.0),
        ("BATT_CAPACITY", 5000.0),
        ("BATT_LOW_VOLT", 14.0),
        ("BATT_LOW_MAH", 0.0),
        ("BATT_CRT_VOLT", 12.8),
        ("BATT_CRT_MAH", 0.0),
        ("BATT_FS_LOW_ACT", 4.0),
        ("BATT_FS_CRT_ACT", 1.0),
        ("EK3_SRC1_POSXY", 3.0),
        ("LAND_SPEED", 50.0),
        ("WPNAV_SPEED", 500.0),
        ("RTL_ALT", 1500.0),
        ("RTL_SPEED", 0.0),
    ]
    .into_iter()
    .map(|(n, v)| (n.to_string(), v))
    .collect()
}

/// The supervised lift-off's hand-over: the airframe model flies the
/// attitude targets until the launch leaves its take-off step airborne.
pub struct LiftoffRig {
    pub af: Airframe,
    pub engaged: bool,
}

pub struct Fc {
    pub mode: u32,
    pub armed: bool,
    /// NED, metres; the ground at `d = 0`, home at the origin.
    pub pos: [f32; 3],
    pub vel: [f32; 3],
    pub att: [f32; 3],
    /// Heading rate, rad/s (body yaw rate, reported in ATTITUDE).
    pub yaw_rate: f32,
    pub landed: bool,
    pub battery_pct: f32,
    /// Battery drain while armed, % per second.
    pub drain_pct_s: f32,
    /// The battery current SYS_STATUS reports while armed, A; `None`: no
    /// current sensor (`current_battery` -1).
    pub current_a: Option<f32>,
    /// SmartRTL retraces the path at this share of the speed the copter flew
    /// it out at in GUIDED (capped by `WPNAV_SPEED`), as the bench showed
    /// (0.80-0.88 round the rig's orbit); `None`: at `WPNAV_SPEED`.
    pub srtl_share: Option<f32>,
    /// Horizontal distance and time flown in GUIDED while moving.
    flown: (f32, f32),
    pub params: BTreeMap<String, f32>,
    pub rig: LiftoffRig,
    /// Mission waypoints, items 1..=n (item 0 is home).
    pub mission: Vec<[f32; 3]>,
    pub seq: u32,
    pub mission_paused: bool,
    pub mission_complete: bool,
    /// The copter makes no progress toward this item (something in the way).
    pub blocked_seq: Option<u32>,
    /// The most horizontal acceleration the position controller flies,
    /// m/s² (ArduCopter's lean-angle limit, `ANGLE_MAX`: 30° is
    /// 9.81·tan 30° = 5.66 m/s²); `None`: the first-order response alone.
    pub max_acc: Option<f32>,
    /// Gust velocity, m/s (north, east).
    gust: [f32; 2],
    /// Per-run gust strength, m/s.
    gust_mps: f32,
    sp: Option<(u64, SET_POSITION_TARGET_LOCAL_NED_DATA)>,
    /// ArduCopter's GUIDED in the detail a position reset needs: a velocity
    /// target flies a position target integrated from it (stabilised, and
    /// held past the setpoint timeout), a position target is kept past the
    /// timeout, a change of submode starts the position controller again at
    /// the copter (dropping any reset offset), and [`Fc::ekf_reset`] moves
    /// a position submode's copter by the step. Off: the simpler model the
    /// other scenarios fly.
    pub ap_guided: bool,
    /// The estimate's steps from [`Fc::ekf_reset`]: the copter truly is at
    /// `pos − frame_shift` ([`Fc::truth`]).
    pub frame_shift: [f32; 3],
    /// ArduPilot's position-controller offset after a reset in a position
    /// submode (`EKFResetMethod::MoveVehicle`), slewed out at the position
    /// controller's speeds.
    offset: [f32; 3],
    /// The velocity submode's integrated position target.
    vel_target: Option<[f32; 2]>,
    hold: [f32; 3],
    takeoff_alt: Option<f32>,
    landed_ms: Option<u64>,
    /// Replies for the pilot, delivered on the next tick.
    replies: Vec<MavMessage>,
    hb_ms: Option<u64>,
    status_ms: Option<u64>,
}

impl Fc {
    pub fn new(rng: &mut Rng) -> Self {
        Self {
            mode: copter::LOITER,
            armed: false,
            pos: [0.0; 3],
            vel: [0.0; 3],
            att: [0.0; 3],
            landed: true,
            battery_pct: 95.0,
            yaw_rate: 0.0,
            drain_pct_s: 0.05,
            current_a: None,
            srtl_share: None,
            flown: (0.0, 0.0),
            params: params(),
            rig: LiftoffRig {
                af: Airframe::balanced(),
                engaged: false,
            },
            mission: Vec::new(),
            seq: 0,
            mission_paused: false,
            mission_complete: false,
            blocked_seq: None,
            gust: [0.0; 2],
            max_acc: None,
            gust_mps: rng.range(0.05, 0.25),
            sp: None,
            ap_guided: false,
            frame_shift: [0.0; 3],
            offset: [0.0; 3],
            vel_target: None,
            hold: [0.0; 3],
            takeoff_alt: None,
            landed_ms: None,
            replies: Vec::new(),
            hb_ms: None,
            status_ms: None,
        }
    }

    fn param(&self, name: &str, default: f32) -> f32 {
        self.params.get(name).copied().unwrap_or(default)
    }

    fn land_mps(&self) -> f32 {
        self.param("LAND_SPEED", 50.0) / 100.0
    }

    fn wp_mps(&self) -> f32 {
        self.param("WPNAV_SPEED", 500.0) / 100.0
    }

    /// SmartRTL's retrace speed ([`Fc::srtl_share`]).
    fn srtl_mps(&self) -> f32 {
        let wp = self.wp_mps();
        match self.srtl_share {
            Some(share) if self.flown.1 > 0.0 => (self.flown.0 / self.flown.1).min(wp) * share,
            _ => wp,
        }
    }

    fn ack(&mut self, command: MavCmd, result: MavResult) {
        self.replies.push(MavMessage::COMMAND_ACK(COMMAND_ACK_DATA {
            command,
            result,
        }));
    }

    fn param_value(&self, name: &str) -> Option<MavMessage> {
        let v = self.params.get(name)?;
        let index = self.params.keys().position(|k| k == name).unwrap_or(0);
        Some(MavMessage::PARAM_VALUE(PARAM_VALUE_DATA {
            param_value: *v,
            param_count: self.params.len() as u16,
            param_index: index as u16,
            param_id: name.into(),
            param_type: MavParamType::MAV_PARAM_TYPE_REAL32,
        }))
    }

    /// Where the copter comes to rest braking from its velocity now: ArduPilot
    /// holds at the stopping point, not where the brake began.
    fn stop_point(&self) -> [f32; 3] {
        let v = self.vel;
        let p = self.pos;
        [
            p[0] + v[0] * VEL_TAU_S,
            p[1] + v[1] * VEL_TAU_S,
            p[2] + v[2] * VEL_TAU_S,
        ]
    }

    fn set_mode(&mut self, mode: u32) {
        if mode != self.mode {
            self.mode = mode;
            self.hold = self.stop_point();
            self.sp = None;
            self.offset = [0.0; 3];
            self.vel_target = None;
            if mode == copter::AUTO && self.seq == 0 && !self.mission.is_empty() {
                self.seq = 1;
            }
        }
    }

    /// Where the copter truly is ([`Fc::frame_shift`]).
    pub fn truth(&self) -> [f32; 3] {
        std::array::from_fn(|i| self.pos[i] - self.frame_shift[i])
    }

    /// The EKF's estimate steps by `d` (north, east, down) while the copter
    /// stays where it is (a source-set change, the origin set in flight, a
    /// GPS glitch, a height source changed). In GUIDED's position submodes
    /// ArduPilot keeps the absolute target and flies the copter to it, the
    /// step slewed in through the offset on each axis
    /// (`ModeGuided::move_vehicle_on_ekf_reset`); the velocity submode, the
    /// take-off and the other modes move their targets with the estimate
    /// (`AC_PosControl::NE_handle_ekf_reset`, `D_handle_ekf_reset`). Needs
    /// [`Fc::ap_guided`].
    pub fn ekf_reset(&mut self, d: [f32; 3]) {
        assert!(self.ap_guided, "a reset is modelled on ArduPilot's GUIDED");
        self.pos = std::array::from_fn(|i| self.pos[i] + d[i]);
        self.frame_shift = std::array::from_fn(|i| self.frame_shift[i] + d[i]);
        let pos_submode =
            self.mode == copter::GUIDED && self.takeoff_alt.is_none() && self.vel_target.is_none();
        if pos_submode {
            self.offset = std::array::from_fn(|i| self.offset[i] + d[i]);
        } else {
            self.hold = std::array::from_fn(|i| self.hold[i] + d[i]);
            if let Some(t) = self.vel_target.as_mut() {
                *t = [t[0] + d[0], t[1] + d[1]];
            }
        }
    }

    /// A setpoint's submode against the one flown: a change starts GUIDED's
    /// position controller again at the copter
    /// (`ModeGuided::pva_control_start`), dropping any reset offset.
    fn guided_submode(&mut self, s: &SET_POSITION_TARGET_LOCAL_NED_DATA) {
        let pos_sp = s.type_mask.bits() & 0b111 == 0;
        let was_pos = self.vel_target.is_none();
        if pos_sp != was_pos {
            self.offset = [0.0; 3];
        }
        self.vel_target = match (pos_sp, self.vel_target) {
            (true, _) => None,
            (false, Some(t)) => Some(t),
            (false, None) => Some([self.pos[0], self.pos[1]]),
        };
    }

    /// One tick of the heading. In GUIDED a setpoint's heading target
    /// (yaw not masked) is turned to, a yaw-rate target (yaw masked, rate
    /// not) flown, while the setpoint is fresh; a position target's heading
    /// is held past the timeout as its position is ([`Fc::ap_guided`]).
    /// Every other case, and every other mode, holds the heading.
    fn yaw_step(&mut self, now: u64, dt: f32) {
        let want = match &self.sp {
            Some((at, s)) if self.mode == copter::GUIDED && self.takeoff_alt.is_none() => {
                let mask = s.type_mask.bits();
                let fresh = now.saturating_sub(*at) <= SETPOINT_TIMEOUT_MS;
                let held = self.ap_guided && mask & 0b111 == 0;
                if mask & MASK_YAW_IGNORE == 0 && (fresh || held) {
                    let err = ados_pilot::geo::wrap_pi(s.yaw - self.att[2]);
                    let slew = YAW_SLEW_DPS.to_radians();
                    (err * YAW_P).clamp(-slew, slew)
                } else if mask & MASK_YAW_RATE_IGNORE == 0 && fresh {
                    let max = YAW_RATE_MAX_DPS.to_radians();
                    s.yaw_rate.clamp(-max, max)
                } else {
                    0.0
                }
            }
            _ => 0.0,
        };
        self.yaw_rate += (want - self.yaw_rate) * (dt / YAW_TC_S).min(1.0);
        self.att[2] = ados_pilot::geo::wrap_pi(self.att[2] + self.yaw_rate * dt);
    }

    /// One tick of [`Fc::ap_guided`]'s GUIDED: the offset slews out at the
    /// position controller's speeds, and the velocity target's position
    /// moves on its velocity (none past the timeout).
    fn ap_guided_step(&mut self, now: u64, dt: f32) {
        let n = self.offset[0].hypot(self.offset[1]);
        if n > 0.0 {
            let k = (n - MAX_H_MPS * dt).max(0.0) / n;
            self.offset = [self.offset[0] * k, self.offset[1] * k, self.offset[2]];
        }
        let z = self.offset[2];
        self.offset[2] = z - z.clamp(-MAX_DOWN_MPS * dt, MAX_DOWN_MPS * dt);
        if let (Some(t), Some((at, s))) = (self.vel_target.as_mut(), &self.sp) {
            if now.saturating_sub(*at) <= SETPOINT_TIMEOUT_MS {
                *t = [t[0] + s.vx * dt, t[1] + s.vy * dt];
            }
        }
    }

    /// The FC's answer to one frame the pilot sent.
    pub fn apply(&mut self, m: &MavMessage, now: u64) {
        match m {
            MavMessage::PARAM_REQUEST_READ(r) => {
                let id = r.param_id.to_str().unwrap_or("").trim_end_matches('\0');
                let reply =
                    self.param_value(id)
                        .unwrap_or(MavMessage::PARAM_ERROR(PARAM_ERROR_DATA {
                            param_index: -1,
                            target_system: 255,
                            target_component: 0,
                            param_id: r.param_id,
                            error: MavParamError::MAV_PARAM_ERROR_DOES_NOT_EXIST,
                        }));
                self.replies.push(reply);
            }
            MavMessage::PARAM_SET(s) => {
                let id = s.param_id.to_str().unwrap_or("").trim_end_matches('\0');
                if self.params.contains_key(id) {
                    self.params.insert(id.to_string(), s.param_value);
                    let reply = self.param_value(id);
                    self.replies.extend(reply);
                }
            }
            MavMessage::COMMAND_LONG(c) => self.command(c),
            MavMessage::COMMAND_INT(c) => self.ack(c.command, MavResult::MAV_RESULT_ACCEPTED),
            MavMessage::SET_POSITION_TARGET_LOCAL_NED(s) if self.mode == copter::GUIDED => {
                if self.rig.engaged {
                    self.rig.af.goal_h = Some(-s.z);
                }
                if self.ap_guided {
                    self.guided_submode(s);
                }
                self.sp = Some((now, s.clone()));
            }
            MavMessage::SET_ATTITUDE_TARGET(t) if self.mode == copter::GUIDED && self.armed => {
                let q = t.q;
                let roll = ados_pilot::geo::atan2(
                    2.0 * (q[0] * q[1] + q[2] * q[3]),
                    1.0 - 2.0 * (q[1] * q[1] + q[2] * q[2]),
                );
                let pitch =
                    ados_pilot::geo::asin((2.0 * (q[0] * q[2] - q[3] * q[1])).clamp(-1.0, 1.0));
                self.rig.af.target = Some(([roll, pitch, self.att[2]], t.thrust));
                self.rig.engaged = true;
            }
            _ => {}
        }
    }

    fn command(&mut self, c: &COMMAND_LONG_DATA) {
        let result = match c.command {
            MavCmd::MAV_CMD_DO_SET_MODE => {
                self.set_mode(c.param2 as u32);
                MavResult::MAV_RESULT_ACCEPTED
            }
            MavCmd::MAV_CMD_COMPONENT_ARM_DISARM => {
                if c.param1 > 0.5 {
                    if self.landed {
                        self.armed = true;
                        self.landed_ms = None;
                        MavResult::MAV_RESULT_ACCEPTED
                    } else {
                        MavResult::MAV_RESULT_FAILED
                    }
                } else {
                    // A plain disarm only on the ground; 21196 forces it.
                    if self.landed || c.param2 == 21196.0 {
                        self.disarm();
                        MavResult::MAV_RESULT_ACCEPTED
                    } else {
                        MavResult::MAV_RESULT_FAILED
                    }
                }
            }
            MavCmd::MAV_CMD_NAV_TAKEOFF if self.armed && self.mode == copter::GUIDED => {
                self.takeoff_alt = Some(c.param7);
                self.hold = [self.pos[0], self.pos[1], -c.param7];
                MavResult::MAV_RESULT_ACCEPTED
            }
            // Disarmed or not in GUIDED: ArduPilot refuses a take-off.
            MavCmd::MAV_CMD_NAV_TAKEOFF => MavResult::MAV_RESULT_FAILED,
            MavCmd::MAV_CMD_DO_PAUSE_CONTINUE => {
                self.mission_paused = c.param1 < 0.5;
                if self.mission_paused {
                    self.hold = self.stop_point();
                }
                MavResult::MAV_RESULT_ACCEPTED
            }
            MavCmd::MAV_CMD_DO_SET_MISSION_CURRENT => {
                let seq = c.param1 as u32;
                if (1..=self.mission.len() as u32).contains(&seq) {
                    self.seq = seq;
                    self.mission_complete = false;
                    MavResult::MAV_RESULT_ACCEPTED
                } else {
                    MavResult::MAV_RESULT_DENIED
                }
            }
            _ => MavResult::MAV_RESULT_ACCEPTED,
        };
        self.ack(c.command, result);
    }

    fn disarm(&mut self) {
        self.armed = false;
        self.takeoff_alt = None;
        self.sp = None;
        if self.rig.engaged {
            // Back on the skids: the FC's integrators relax to zero.
            self.rig.af = Airframe::new(self.rig.af.hover, self.rig.af.trim);
            self.rig.engaged = false;
        }
    }

    /// The velocity the FC's own controller asks for in its current mode.
    fn target_vel(&mut self, now: u64) -> [f32; 3] {
        let toward = |from: [f32; 3], to: [f32; 3], speed: f32| -> [f32; 3] {
            let d = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
            let h = (d[0] * d[0] + d[1] * d[1]).sqrt();
            let s = if h > 1e-3 {
                (speed.min(h * POS_GAIN)) / h
            } else {
                0.0
            };
            [d[0] * s, d[1] * s, d[2] * POS_GAIN]
        };
        let hold = toward(self.pos, self.hold, MAX_H_MPS);
        match self.mode {
            copter::GUIDED => {
                if let Some(alt) = self.takeoff_alt {
                    if -self.pos[2] >= alt - 0.1 {
                        self.takeoff_alt = None;
                        self.hold = self.pos;
                    } else {
                        return [hold[0], hold[1], -TAKEOFF_MPS];
                    }
                }
                match &self.sp {
                    Some((at, s)) if self.ap_guided => {
                        let mask = s.type_mask.bits();
                        let mut v = [0.0; 3];
                        if mask & 0b111_000 == 0 && now.saturating_sub(*at) <= SETPOINT_TIMEOUT_MS {
                            v = [s.vx, s.vy, s.vz];
                        }
                        if mask & 0b111 == 0 {
                            v[0] += (s.x + self.offset[0] - self.pos[0]) * POS_GAIN;
                            v[1] += (s.y + self.offset[1] - self.pos[1]) * POS_GAIN;
                            v[2] += (s.z + self.offset[2] - self.pos[2]) * POS_GAIN;
                        } else if let Some(t) = self.vel_target {
                            v[0] += (t[0] - self.pos[0]) * POS_GAIN;
                            v[1] += (t[1] - self.pos[1]) * POS_GAIN;
                        }
                        v
                    }
                    None if self.ap_guided => {
                        let to = [
                            self.hold[0] + self.offset[0],
                            self.hold[1] + self.offset[1],
                            self.hold[2] + self.offset[2],
                        ];
                        toward(self.pos, to, MAX_H_MPS)
                    }
                    Some((at, s)) if now.saturating_sub(*at) <= SETPOINT_TIMEOUT_MS => {
                        let mask = s.type_mask.bits();
                        let mut v = [0.0; 3];
                        if mask & 0b111_000 == 0 {
                            v = [s.vx, s.vy, s.vz];
                        }
                        if mask & 0b111 == 0 {
                            v[0] += (s.x - self.pos[0]) * POS_GAIN;
                            v[1] += (s.y - self.pos[1]) * POS_GAIN;
                            v[2] += (s.z - self.pos[2]) * POS_GAIN;
                        }
                        v
                    }
                    _ => hold,
                }
            }
            copter::AUTO => {
                let wp = (self.seq as usize)
                    .checked_sub(1)
                    .and_then(|i| self.mission.get(i))
                    .copied();
                match wp {
                    Some(wp)
                        if !self.mission_paused
                            && !self.mission_complete
                            && self.blocked_seq != Some(self.seq) =>
                    {
                        let d = [
                            wp[0] - self.pos[0],
                            wp[1] - self.pos[1],
                            wp[2] - self.pos[2],
                        ];
                        if (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt() < WP_RADIUS_M {
                            self.hold = wp;
                            if self.seq as usize >= self.mission.len() {
                                self.mission_complete = true;
                            } else {
                                self.seq += 1;
                            }
                        }
                        toward(self.pos, wp, self.wp_mps())
                    }
                    // Paused, done, blocked or nothing to fly: AUTO holds.
                    _ => hold,
                }
            }
            copter::RTL | copter::SMART_RTL => {
                let rtl_alt = self.param("RTL_ALT", 1500.0) / 100.0;
                if self.mode == copter::RTL && -self.pos[2] < rtl_alt - 0.2 && !self.landed {
                    return [0.0, 0.0, -MAX_UP_MPS];
                }
                // Straight home at the current height, then down.
                let v = toward(self.pos, [0.0, 0.0, self.pos[2]], self.srtl_mps());
                let home_m = (self.pos[0] * self.pos[0] + self.pos[1] * self.pos[1]).sqrt();
                let vz = if home_m > 0.5 { 0.0 } else { self.land_mps() };
                [v[0], v[1], vz]
            }
            copter::LAND => [hold[0], hold[1], self.land_mps()],
            // LOITER, BRAKE, POSHOLD and the rest: hold where the mode began.
            _ => hold,
        }
    }

    /// Advance one tick of `dt` seconds. `liftoff_takeoff`: the pilot's
    /// launch is still in its take-off step.
    pub fn step(&mut self, now: u64, dt: f32, rng: &mut Rng, liftoff_takeoff: bool) {
        self.gust = [
            self.gust[0] * 0.98 + rng.noise(self.gust_mps * 0.1),
            self.gust[1] * 0.98 + rng.noise(self.gust_mps * 0.1),
        ];
        self.rig.af.armed = self.armed;
        if !self.armed {
            self.vel = [0.0; 3];
            self.yaw_rate = 0.0;
            self.pos[2] = 0.0;
            self.att = [0.0, 0.0, self.att[2]];
            self.landed = true;
            return;
        }
        self.battery_pct = (self.battery_pct - self.drain_pct_s * dt).max(0.0);
        if self.rig.engaged && self.mode != copter::GUIDED {
            // Out of GUIDED: the FC's own mode flies from here.
            self.rig.engaged = false;
            self.hold = self.pos;
        }
        if self.rig.engaged {
            if !liftoff_takeoff && self.rig.af.airborne() {
                // Handed on: the position controller flies from where the
                // airframe is, at its climb rate.
                self.rig.engaged = false;
                self.vel = [0.0, 0.0, -self.rig.af.vz];
                self.hold = self.pos;
            } else {
                for _ in 0..4 {
                    self.rig.af.step(dt / 4.0);
                }
                let af = &self.rig.af;
                self.att = [af.att[0], af.att[1], self.att[2]];
                self.pos[2] = -af.h;
                self.vel = [0.0, 0.0, -af.vz];
                self.landed = !af.airborne();
                return;
            }
        }
        if self.ap_guided && self.mode == copter::GUIDED {
            self.ap_guided_step(now, dt);
        }
        if !self.landed {
            self.yaw_step(now, dt);
        } else {
            self.yaw_rate = 0.0;
        }
        let want = self.target_vel(now);
        let h = (want[0] * want[0] + want[1] * want[1]).sqrt();
        let hs = if h > MAX_H_MPS { MAX_H_MPS / h } else { 1.0 };
        let want = [
            want[0] * hs,
            want[1] * hs,
            want[2].clamp(-MAX_UP_MPS, MAX_DOWN_MPS),
        ];
        let k = (dt / VEL_TAU_S).min(1.0);
        let before = self.vel;
        for (v, w) in self.vel.iter_mut().zip(want) {
            *v += (w - *v) * k;
        }
        if let Some(max) = self.max_acc {
            let dv = [self.vel[0] - before[0], self.vel[1] - before[1]];
            let a = dv[0].hypot(dv[1]) / dt;
            if a > max {
                let s = max / a;
                self.vel[0] = before[0] + dv[0] * s;
                self.vel[1] = before[1] + dv[1] * s;
            }
        }
        let acc = [
            (self.vel[0] - before[0]) / dt,
            (self.vel[1] - before[1]) / dt,
        ];
        // The lean that flies that acceleration, in the body frame: pitch
        // nose down to speed up along the heading, roll right to speed up to
        // the right.
        let (sy, cy) = self.att[2].sin_cos();
        let fwd = acc[0] * cy + acc[1] * sy;
        let right = -acc[0] * sy + acc[1] * cy;
        self.att = [(right / G).atan(), -(fwd / G).atan(), self.att[2]];
        let flying = !self.landed || self.vel[2] < -0.05 || self.takeoff_alt.is_some();
        if flying {
            self.pos[0] += (self.vel[0] + self.gust[0]) * dt;
            self.pos[1] += (self.vel[1] + self.gust[1]) * dt;
            self.pos[2] += self.vel[2] * dt;
        }
        let v_h = self.vel[0].hypot(self.vel[1]);
        if self.mode == copter::GUIDED && v_h > 0.5 {
            self.flown.0 += v_h * dt;
            self.flown.1 += dt;
        }
        if self.pos[2] >= 0.0 {
            self.pos[2] = 0.0;
            self.vel = [0.0; 3];
            self.landed = true;
        } else if self.pos[2] < -0.05 {
            self.landed = false;
        }
        let lands = matches!(self.mode, copter::LAND | copter::RTL | copter::SMART_RTL);
        if self.landed && lands {
            let since = *self.landed_ms.get_or_insert(now);
            if now.saturating_sub(since) >= LAND_DISARM_MS {
                self.disarm();
                self.landed_ms = None;
            }
        } else {
            self.landed_ms = None;
        }
    }

    /// The frames the FC sends this tick: replies first, then its streams.
    pub fn frames(&mut self, now: u64, rng: &mut Rng) -> Vec<MavMessage> {
        let mut out = std::mem::take(&mut self.replies);
        if self
            .hb_ms
            .is_none_or(|t| now.saturating_sub(t) >= HEARTBEAT_MS)
        {
            self.hb_ms = Some(now);
            let mut base = MavModeFlag::MAV_MODE_FLAG_CUSTOM_MODE_ENABLED;
            if self.armed {
                base |= MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED;
            }
            out.push(MavMessage::HEARTBEAT(HEARTBEAT_DATA {
                custom_mode: self.mode,
                mavtype: MavType::MAV_TYPE_QUADROTOR,
                autopilot: MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
                base_mode: base,
                system_status: MavState::MAV_STATE_ACTIVE,
                mavlink_version: 3,
            }));
            let current = if self.armed { self.current_a } else { None };
            out.push(MavMessage::SYS_STATUS(SYS_STATUS_DATA {
                voltage_battery: ((12.8 + 4.0 * self.battery_pct / 100.0) * 1000.0) as u16,
                battery_remaining: self.battery_pct.floor() as i8,
                current_battery: current.map_or(-1, |a| (a * 100.0).round() as i16),
                ..Default::default()
            }));
        }
        if self
            .status_ms
            .is_none_or(|t| now.saturating_sub(t) >= STATUS_MS)
        {
            self.status_ms = Some(now);
            out.push(MavMessage::EXTENDED_SYS_STATE(EXTENDED_SYS_STATE_DATA {
                vtol_state: MavVtolState::MAV_VTOL_STATE_UNDEFINED,
                landed_state: if self.landed {
                    MavLandedState::MAV_LANDED_STATE_ON_GROUND
                } else {
                    MavLandedState::MAV_LANDED_STATE_IN_AIR
                },
            }));
            out.push(MavMessage::GPS_RAW_INT(GPS_RAW_INT_DATA {
                fix_type: GpsFixType::GPS_FIX_TYPE_3D_FIX,
                satellites_visible: 14,
                ..Default::default()
            }));
        }
        let pos_sd = 0.01;
        let vel_sd = 0.02;
        out.push(MavMessage::LOCAL_POSITION_NED(LOCAL_POSITION_NED_DATA {
            time_boot_ms: now as u32,
            x: self.pos[0] + rng.noise(pos_sd),
            y: self.pos[1] + rng.noise(pos_sd),
            z: self.pos[2] + rng.noise(pos_sd),
            vx: self.vel[0] + rng.noise(vel_sd),
            vy: self.vel[1] + rng.noise(vel_sd),
            vz: self.vel[2] + rng.noise(vel_sd),
        }));
        out.push(MavMessage::ATTITUDE(ATTITUDE_DATA {
            time_boot_ms: now as u32,
            roll: self.att[0],
            pitch: self.att[1],
            yaw: self.att[2],
            rollspeed: 0.0,
            pitchspeed: 0.0,
            yawspeed: self.yaw_rate,
        }));
        let agl = SKID_M - self.pos[2] + rng.noise(0.005);
        out.push(MavMessage::DISTANCE_SENSOR(DISTANCE_SENSOR_DATA {
            time_boot_ms: now as u32,
            min_distance: 5,
            max_distance: 1200,
            current_distance: (agl.max(0.0) * 100.0).round() as u16,
            mavtype: MavDistanceSensor::MAV_DISTANCE_SENSOR_LASER,
            id: 0,
            orientation: MavSensorOrientation::MAV_SENSOR_ROTATION_PITCH_270,
            covariance: 0,
        }));
        if self.armed {
            let af = &self.rig.af;
            for (axis, i) in [
                (PidTuningAxis::PID_TUNING_ROLL, af.i[0]),
                (PidTuningAxis::PID_TUNING_PITCH, af.i[1]),
            ] {
                out.push(MavMessage::PID_TUNING(PID_TUNING_DATA {
                    I: i,
                    axis,
                    ..Default::default()
                }));
            }
            let o = af.motors();
            out.push(MavMessage::SERVO_OUTPUT_RAW(SERVO_OUTPUT_RAW_DATA {
                servo1_raw: o[0],
                servo2_raw: o[1],
                servo3_raw: o[2],
                servo4_raw: o[3],
                ..Default::default()
            }));
        }
        out
    }
}
