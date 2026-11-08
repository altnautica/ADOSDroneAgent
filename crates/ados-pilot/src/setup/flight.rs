//! Take-off, hold and landing sequences the air-wizard steps share.
//!
//! The setup engine holds owner `tuning` for the whole air wizard, so these
//! sequences are the only setpoint source while they run.

use ados_protocol::mavlink::ardupilotmega::{MavCmd, MavFrame, MavVtolState};

use super::adapters::Mode;
use super::fcstate::{Class, Fw};
use super::step::Cx;

/// Re-send interval for a mode, arm or take-off command not yet effective.
const RESEND_MS: u64 = 1500;
/// Hold setpoint refresh.
const HOLD_MS: u64 = 500;
/// Give up on a take-off after this long.
const TAKEOFF_TIMEOUT_MS: u64 = 45_000;
/// Give up on a landing after this long.
const LAND_TIMEOUT_MS: u64 = 90_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Leg {
    Idle,
    /// Switching to the take-off mode and arming.
    Arming,
    TakingOff,
    /// Airborne at the target height, holding.
    Holding,
    Landing,
    Landed,
    Failed(&'static str),
}

/// One take-off → hold → land cycle.
#[derive(Debug, Clone)]
pub struct Hop {
    pub leg: Leg,
    leg_ms: u64,
    sent_ms: u64,
    alt_m: f32,
    /// Hold point `(n, e, d)` and yaw (rad) for copter holds.
    pub hold: Option<([f32; 3], f32)>,
}

impl Default for Hop {
    fn default() -> Self {
        Hop { leg: Leg::Idle, leg_ms: 0, sent_ms: 0, alt_m: 0.0, hold: None }
    }
}

/// Height above the take-off point (m).
pub fn height(cx: &Cx) -> Option<f32> {
    cx.fc.rel_alt_m.or_else(|| cx.fc.local.map(|(l, _)| -l[2]))
}

/// The vehicle is airborne.
pub fn airborne(cx: &Cx) -> bool {
    cx.fc.armed && !cx.fc.on_ground() && height(cx).is_some_and(|h| h > 0.5)
}

/// The mode a take-off uses: GUIDED take-off on copters and QuadPlanes,
/// TAKEOFF mode on a fixed-wing, OFFBOARD climb on PX4.
fn takeoff_mode(cx: &Cx) -> Mode {
    match (cx.adapter.fw, cx.adapter.class) {
        (Fw::Px4, _) => Mode::Offboard,
        (_, Class::Fixedwing) => Mode::Takeoff,
        _ => Mode::Guided,
    }
}

pub fn set_mode(cx: &mut Cx, m: Mode) {
    if let Some((main, sub)) = cx.adapter.mode(m) {
        cx.out.set_mode(main, sub);
    }
}

pub fn in_mode(cx: &Cx, m: Mode) -> bool {
    cx.adapter.in_mode(cx.fc.custom_mode, m)
}

impl Hop {
    pub fn start(&mut self, cx: &mut Cx, alt_m: f32) {
        self.alt_m = alt_m;
        self.hold = None;
        self.leg_ms = cx.now;
        self.sent_ms = 0;
        self.leg = if airborne(cx) { Leg::Holding } else { Leg::Arming };
        if self.leg == Leg::Holding {
            self.capture_hold(cx);
        }
        if cx.data.box_center.is_none() {
            cx.data.box_center = cx.fc.local.map(|(l, _)| [l[0], l[1]]);
        }
        if cx.data.box_latlon.is_none() {
            cx.data.box_latlon = cx.fc.latlon;
        }
    }

    pub fn land(&mut self, cx: &mut Cx) {
        self.leg = Leg::Landing;
        self.leg_ms = cx.now;
        self.sent_ms = 0;
    }

    fn capture_hold(&mut self, cx: &Cx) {
        if let (Some((l, _)), Some((a, _))) = (cx.fc.local, cx.fc.attitude) {
            self.hold = Some(([l[0], l[1], l[2]], a[2]));
        }
    }

    fn resend(&mut self, cx: &Cx) -> bool {
        if self.sent_ms == 0 || cx.now.saturating_sub(self.sent_ms) >= RESEND_MS {
            self.sent_ms = cx.now;
            true
        } else {
            false
        }
    }

    /// Move the hold point (copter): the next hold setpoints fly there.
    pub fn set_hold(&mut self, n: f32, e: f32, d: f32, yaw: f32) {
        self.hold = Some(([n, e, d], yaw));
    }

    pub fn tick(&mut self, cx: &mut Cx) {
        match self.leg {
            Leg::Idle | Leg::Landed | Leg::Failed(_) => {}
            Leg::Arming => {
                let m = takeoff_mode(cx);
                if cx.now.saturating_sub(self.leg_ms) > TAKEOFF_TIMEOUT_MS {
                    self.leg = Leg::Failed("takeoff_timeout");
                    return;
                }
                if cx.adapter.fw == Fw::Px4 {
                    // OFFBOARD needs setpoints streaming before the switch.
                    if let Some((l, _)) = cx.fc.local {
                        let yaw = cx.fc.attitude.map(|(a, _)| a[2]).unwrap_or(0.0);
                        cx.out.hold_position(l[0], l[1], -self.alt_m, yaw);
                    }
                }
                if !self.resend(cx) {
                    return;
                }
                if !in_mode(cx, m) {
                    set_mode(cx, m);
                } else if !cx.fc.armed {
                    cx.out.arm(true);
                } else {
                    if cx.adapter.class != Class::Fixedwing && cx.adapter.fw != Fw::Px4 {
                        cx.out.takeoff(self.alt_m);
                    }
                    self.leg = Leg::TakingOff;
                    self.leg_ms = cx.now;
                }
            }
            Leg::TakingOff => {
                let h = height(cx).unwrap_or(0.0);
                if h >= self.alt_m * 0.9 {
                    self.leg = Leg::Holding;
                    self.leg_ms = cx.now;
                    self.capture_hold(cx);
                    if cx.adapter.class == Class::Fixedwing && cx.adapter.fw == Fw::ArduPilot {
                        // TAKEOFF done: GUIDED loiters at the current point.
                        set_mode(cx, Mode::Guided);
                    }
                    return;
                }
                if cx.now.saturating_sub(self.leg_ms) > TAKEOFF_TIMEOUT_MS {
                    self.leg = Leg::Failed("takeoff_timeout");
                    return;
                }
                if cx.adapter.fw == Fw::Px4 {
                    if let Some((l, _)) = cx.fc.local {
                        let yaw = cx.fc.attitude.map(|(a, _)| a[2]).unwrap_or(0.0);
                        cx.out.hold_position(l[0], l[1], -self.alt_m, yaw);
                    }
                } else if h < 0.3 && self.resend(cx) && cx.adapter.class != Class::Fixedwing {
                    // Not climbing yet: the take-off was not taken; repeat it.
                    if cx.now.saturating_sub(self.leg_ms) > 3000 {
                        cx.out.takeoff(self.alt_m);
                    }
                }
                if !cx.fc.armed && cx.now.saturating_sub(self.leg_ms) > 3000 {
                    self.leg = Leg::Failed("disarmed");
                }
            }
            Leg::Holding => {
                if !cx.fc.armed {
                    self.leg = Leg::Failed("disarmed");
                    return;
                }
                if (cx.adapter.copter_like() || cx.adapter.fw == Fw::Px4) && cx.now.saturating_sub(self.sent_ms) >= HOLD_MS {
                    self.sent_ms = cx.now;
                    if let Some((p, yaw)) = self.hold {
                        cx.out.hold_position(p[0], p[1], p[2], yaw);
                    }
                }
            }
            Leg::Landing => {
                if !cx.fc.armed || (cx.fc.on_ground() && cx.fc.landed.is_some()) {
                    if !cx.fc.armed {
                        self.leg = Leg::Landed;
                    } else if self.resend(cx) {
                        cx.out.arm(false);
                    }
                    return;
                }
                if cx.now.saturating_sub(self.leg_ms) > LAND_TIMEOUT_MS {
                    self.leg = Leg::Failed("land_timeout");
                    return;
                }
                let m = land_mode(cx);
                if !in_mode(cx, m) && self.resend(cx) {
                    set_mode(cx, m);
                }
            }
        }
    }
}

/// How this vehicle lands: LAND on a copter, QLAND on a QuadPlane, RTL on
/// a fixed-wing (it loiters at home unless a landing is planned).
pub fn land_mode(cx: &Cx) -> Mode {
    match cx.adapter.class {
        Class::Fixedwing => Mode::Rtl,
        _ => Mode::Land,
    }
}

/// Fixed-wing loiter height for the air wizard (m above take-off).
pub const PLANE_LOITER_ALT_M: f32 = 80.0;

/// Fixed-wing: loiter at the tuning-box centre at `alt_m` (DO_REPOSITION,
/// which also switches to GUIDED). The height is sent again as
/// GUIDED_CHANGE_ALTITUDE: once a step has set one (the TECS stages),
/// ArduPlane's GUIDED keeps flying that offboard height and ignores the
/// reposition's (`ModeGuided::update_target_altitude`).
pub fn plane_loiter(cx: &mut Cx, alt_m: f32) {
    let (lat, lon) = cx.data.box_latlon.or(cx.fc.latlon).unwrap_or((0, 0));
    cx.out.cmd_int(
        MavCmd::MAV_CMD_DO_REPOSITION,
        MavFrame::MAV_FRAME_GLOBAL_RELATIVE_ALT,
        [-1.0, 1.0, 0.0, f32::NAN],
        lat,
        lon,
        alt_m,
    );
    change_alt(cx, alt_m, 0.0);
}

/// Fixed-wing GUIDED: new target height (m, relative) at `rate` m/s (0 = FC default).
pub fn change_alt(cx: &mut Cx, alt_m: f32, rate: f32) {
    cx.out.cmd_int(MavCmd::MAV_CMD_GUIDED_CHANGE_ALTITUDE, MavFrame::MAV_FRAME_GLOBAL_RELATIVE_ALT, [0.0, 0.0, rate, 0.0], 0, 0, alt_m);
}

/// Fixed-wing GUIDED: new target airspeed (m/s).
pub fn change_speed(cx: &mut Cx, airspeed: f32) {
    cx.out.cmd_int(MavCmd::MAV_CMD_GUIDED_CHANGE_SPEED, MavFrame::MAV_FRAME_GLOBAL, [0.0, airspeed, 0.0, 0.0], 0, 0, 0.0);
}

/// VTOL transitions by mode switch: ArduPlane refuses
/// MAV_CMD_DO_VTOL_TRANSITION outside AUTO (`QuadPlane::handle_do_vtol_transition`).
/// Forward: `Q_GUIDED_MODE` = 0 then a GUIDED fixed-wing loiter; back:
/// QLOITER, then `Q_GUIDED_MODE` = 1 again (the ADOS profile value).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Transition {
    Idle,
    ToFw { since: u64, sent: u64 },
    ToMc { since: u64, sent: u64 },
    Done,
    Failed,
}

const TRANSITION_TIMEOUT_MS: u64 = 40_000;

impl Transition {
    pub fn to_fw(cx: &mut Cx) -> Self {
        cx.params.set("Q_GUIDED_MODE", 0.0);
        Transition::ToFw { since: cx.now, sent: 0 }
    }

    pub fn to_mc(cx: &mut Cx) -> Self {
        Transition::ToMc { since: cx.now, sent: 0 }
    }

    pub fn tick(&mut self, cx: &mut Cx) {
        match *self {
            Transition::ToFw { since, sent } => {
                if cx.fc.vtol_state == Some(MavVtolState::MAV_VTOL_STATE_FW) {
                    *self = Transition::Done;
                } else if cx.now.saturating_sub(since) > TRANSITION_TIMEOUT_MS {
                    *self = Transition::Failed;
                } else if cx.params.idle() && cx.now.saturating_sub(sent) > 3000 {
                    plane_loiter(cx, PLANE_LOITER_ALT_M);
                    *self = Transition::ToFw { since, sent: cx.now };
                }
            }
            Transition::ToMc { since, sent } => {
                if cx.fc.vtol_state == Some(MavVtolState::MAV_VTOL_STATE_MC) {
                    cx.params.set("Q_GUIDED_MODE", 1.0);
                    *self = Transition::Done;
                } else if cx.now.saturating_sub(since) > TRANSITION_TIMEOUT_MS {
                    *self = Transition::Failed;
                } else if cx.now.saturating_sub(sent) > 3000 {
                    set_mode(cx, Mode::Qloiter);
                    *self = Transition::ToMc { since, sent: cx.now };
                }
            }
            _ => {}
        }
    }
}
