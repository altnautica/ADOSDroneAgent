//! The air wizard's take-off on an ArduPilot multirotor: the supervised
//! lift-off ([`crate::liftoff`]) instead of the FC's own NAV_TAKEOFF, with
//! the FC set up the way the pilot's launch sets it up (`GUID_OPTIONS`
//! bit 3, `GCS_PID_MASK` roll and pitch, PID_TUNING and SERVO_OUTPUT_RAW at
//! 25 Hz).
//!
//! The lift-off ramps against the pilot's hover estimate: the airframe's
//! learned hover, else `MOT_THST_HOVER`. On the `first_hover` step of a new
//! airframe that is the FC's untuned default, which the step itself measures.
//! A stop on the ground fails the hop with the stop's code; what the
//! lift-off found goes to the pilot's airframe health as an effect.

use crate::fc::{Attitude, LocalPos};
use crate::liftoff::{Cmd, Inputs, Liftoff, Motors, Stop, GCS_PID_ROLL_PITCH, GUID_OPTIONS_THRUST};
use crate::setup::adapters::Mode;
use crate::setup::fcstate::{Class, Fw};
use crate::setup::out::Effect;
use crate::setup::step::Cx;

use super::in_mode;

/// The FC setup is re-sent this often until the FC reports it…
const PREP_RESEND_MS: u64 = 1000;
/// …and the hop fails without it after this long.
const PREP_TIMEOUT_MS: u64 = 6000;
/// The rate-loop and motor streams the lift-off reads: PID_TUNING and
/// SERVO_OUTPUT_RAW, at this rate.
const STREAMS: [u32; 2] = [194, 36];
const STREAM_HZ: f32 = 25.0;
/// FC data older than this is not used.
const FRESH_MS: u64 = 500;
/// A stopped lift-off holds thrust at zero this long before the disarm.
const SPINDOWN_MS: u64 = 300;
/// Ground contact: no higher than at rest by more than this (m), and not
/// climbing faster than this (m/s).
const GROUND_RISE_M: f32 = 0.05;
const GROUND_VZ_MPS: f32 = 0.2;
/// The take-off gives up after this long: the lift-off's own ground limit
/// plus the climb.
pub const TAKEOFF_TIMEOUT_MS: u64 = 120_000;

/// This vehicle's take-off is the supervised lift-off.
pub fn applies(cx: &Cx) -> bool {
    cx.inputs.supervised_liftoff
        && cx.adapter.fw == Fw::ArduPilot
        && cx.adapter.class == Class::Multirotor
}

/// One tick's result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Lift {
    Lifting,
    /// Off the ground, the position target climbing to the hop's height.
    HandedOver,
    /// Stopped on the ground and spun down: the hop fails with this code.
    Stopped(&'static str),
}

/// A supervised take-off in progress.
#[derive(Debug, Clone, Default)]
pub struct Supervised {
    /// When the FC setup began, and when it was last sent.
    prep_since: Option<u64>,
    prep_sent: u64,
    lo: Option<Liftoff>,
    stopped_ms: Option<u64>,
    disarm_sent: bool,
}

impl Supervised {
    /// Before arming: `Ok(true)` once the FC reports both bits and its
    /// `MOT_THST_HOVER`. The bits are ORed into the FC's own values.
    pub fn prepared(&mut self, cx: &mut Cx) -> Result<bool, &'static str> {
        let has = |n: &str, bits: u32| cx.params.get(n).is_some_and(|v| (v as u32) & bits == bits);
        if has("GUID_OPTIONS", GUID_OPTIONS_THRUST)
            && has("GCS_PID_MASK", GCS_PID_ROLL_PITCH)
            && cx.params.has("MOT_THST_HOVER")
        {
            return Ok(true);
        }
        let since = *self.prep_since.get_or_insert(cx.now);
        if cx.now.saturating_sub(since) > PREP_TIMEOUT_MS {
            return Err("liftoff_fc_params");
        }
        if self.prep_sent != 0 && cx.now.saturating_sub(self.prep_sent) < PREP_RESEND_MS {
            return Ok(false);
        }
        self.prep_sent = cx.now.max(1);
        for (name, bits) in [
            ("GUID_OPTIONS", GUID_OPTIONS_THRUST),
            ("GCS_PID_MASK", GCS_PID_ROLL_PITCH),
        ] {
            match cx.params.get(name) {
                Some(v) => cx.params.set(name, ((v as u32) | bits) as f32),
                None => cx.params.read(name),
            }
        }
        if !cx.params.has("MOT_THST_HOVER") {
            cx.params.read("MOT_THST_HOVER");
        }
        streams(cx);
        Ok(false)
    }

    /// Armed and set up: the lift-off to `alt_m` begins.
    pub fn begin(&mut self, cx: &mut Cx, alt_m: f32) {
        streams(cx);
        self.lo = Some(Liftoff::new(
            cx.inputs.liftoff_trim,
            cx.inputs.liftoff_hover,
            alt_m,
            cx.now,
        ));
    }

    /// Stopped on the ground.
    pub fn stopped(&self) -> bool {
        self.lo.as_ref().is_some_and(|lo| lo.stopped().is_some())
    }

    /// One tick of the lift-off: the frame it asks for, or the spin-down
    /// once it stopped.
    pub fn tick(&mut self, cx: &mut Cx) -> Lift {
        let inp = inputs(cx);
        let Some(lo) = self.lo.as_mut() else {
            return Lift::Lifting;
        };
        if cx.inputs.link_lost {
            lo.stop(Stop::Link);
        }
        let cmd = lo.step(&inp);
        if let Some(o) = lo.take_outcome() {
            cx.out.effect(Effect::LiftoffOutcome(o));
        }
        if let Some(stop) = lo.stopped() {
            return self.spindown(cx, stop, &inp);
        }
        match cmd {
            Some(Cmd::Attitude {
                roll,
                pitch,
                yaw,
                thrust,
            }) => cx.out.attitude_target(roll, pitch, yaw, thrust),
            Some(Cmd::Position { pos, vel, yaw }) => cx.out.position_moving(pos, vel, yaw),
            None => {}
        }
        if lo.handed_over() {
            Lift::HandedOver
        } else {
            Lift::Lifting
        }
    }

    /// Thrust at zero at the attitude it has, then a forced disarm on
    /// confirmed ground contact, else the FC's Land. Done once the FC has
    /// disarmed or left GUIDED.
    fn spindown(&mut self, cx: &mut Cx, stop: Stop, inp: &Inputs) -> Lift {
        let ground = self.ground_contact(inp);
        let since = *self.stopped_ms.get_or_insert(cx.now);
        if self.disarm_sent {
            return if cx.fc.armed && in_mode(cx, Mode::Guided) {
                Lift::Lifting
            } else {
                Lift::Stopped(stop.code())
            };
        }
        if cx.now.saturating_sub(since) < SPINDOWN_MS {
            if let Some(a) = inp.att {
                cx.out.attitude_target(a.roll, a.pitch, a.yaw, 0.0);
            }
            return Lift::Lifting;
        }
        self.disarm_sent = true;
        tracing::warn!(reason = stop.code(), ground, "fc_setup_liftoff_stopped");
        if ground {
            cx.out.force_disarm();
        } else {
            super::set_mode(cx, super::land_mode(cx));
        }
        Lift::Lifting
    }

    /// Never lifted, no higher than at rest and not climbing, and the
    /// rangefinder (when fitted) reading no more than at rest.
    fn ground_contact(&self, inp: &Inputs) -> bool {
        let Some(lo) = self.lo.as_ref() else {
            return false;
        };
        let (Some(rest), Some(now)) = (lo.start(), inp.local) else {
            return false;
        };
        let range_ok = match (lo.rest_agl(), inp.agl) {
            (Some(r0), Some(r)) => r - r0 <= GROUND_RISE_M,
            _ => true,
        };
        !lo.lifted_ever()
            && rest.d - now.d <= GROUND_RISE_M
            && now.vd.abs() <= GROUND_VZ_MPS
            && range_ok
    }
}

fn streams(cx: &mut Cx) {
    for id in STREAMS {
        cx.out.message_interval(id, STREAM_HZ);
    }
}

/// What the FC reports this tick, in the lift-off's terms.
fn inputs(cx: &Cx) -> Inputs {
    let now = cx.now;
    let fresh = |t: u64| now.saturating_sub(t) < FRESH_MS;
    let n = match cx.params.get("FRAME_CLASS").map(|v| v as u32) {
        Some(2 | 5) => 6,
        Some(3 | 4 | 12 | 14) => 8,
        Some(7) => 3,
        _ => 4,
    };
    let pwm = |k: &str, d: f32| cx.params.get(k).filter(|v| *v > 0.0).unwrap_or(d);
    let (lo, hi) = (pwm("MOT_PWM_MIN", 1000.0), pwm("MOT_PWM_MAX", 2000.0));
    let motors = cx
        .fc
        .loop_rates
        .fresh_motors(n, now, FRESH_MS)
        .filter(|o| !o.is_empty() && hi > lo)
        .map(|o| {
            let max = *o.iter().max().unwrap_or(&0);
            let min = *o.iter().min().unwrap_or(&0);
            let frac = |v: u16| ((f32::from(v) - lo) / (hi - lo)).clamp(0.0, 1.0);
            Motors {
                max: frac(max),
                min: frac(min),
                spread_us: max - min,
            }
        });
    Inputs {
        now_ms: now,
        att: cx
            .fc
            .attitude
            .filter(|(_, t)| fresh(*t))
            .map(|(a, _)| Attitude {
                roll: a[0],
                pitch: a[1],
                yaw: a[2],
                yaw_rate: 0.0,
            }),
        local: cx
            .fc
            .local
            .filter(|(_, t)| fresh(*t))
            .map(|(l, _)| LocalPos {
                n: l[0],
                e: l[1],
                d: l[2],
                vn: l[3],
                ve: l[4],
                vd: l[5],
            }),
        i: cx.fc.loop_rates.fresh_i(now, FRESH_MS),
        motors,
        agl: cx.fc.rangefinder_agl(now),
    }
}
