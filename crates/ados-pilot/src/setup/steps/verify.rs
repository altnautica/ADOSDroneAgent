//! `verify`: fly the tuned aircraft and compare with the first flight.
//!
//! Copter (and VTOL in hover, PX4): a 10 s hover, then a velocity box of
//! 3 m/s steps forward, back, left and right for 3 s each. Plane (and VTOL
//! in forward flight): bank steps of ±30° (FBWA) and climb steps of ±2 m/s
//! (GUIDED). Attitude tracking error is compared with the first flight; any
//! axis worse by more than 20 % offers "Revert gains" (the parameter
//! snapshot's gain values).

use serde_json::{json, Map, Value};

use crate::setup::adapters::Mode;
use crate::setup::fcstate::{Class, Fw};
use crate::setup::flight::{self, height, plane_loiter, Hop, Leg, Transition, PLANE_LOITER_ALT_M};
use crate::setup::measure::{overshoot, Acc};
use crate::setup::params::parse_param_file;
use crate::setup::schema::Action;
use crate::setup::step::{step_core, writes_settled, ActErr, Cx, Step, StepCore};

use super::plane_air::ToLoiter;

pub const BOX_SPEED: f32 = 3.0;
pub const BOX_LEG_MS: u64 = 3000;
const BOX_BRAKE_MS: u64 = 2000;
pub const HOVER_MS: u64 = 10_000;
pub const VERIFY_ALT_M: f32 = 3.0;
/// Worse than the first flight by more than this fraction offers a revert.
pub const WORSE: f32 = 0.20;
pub const BANK_DEG: f32 = 30.0;
const BANK_MS: u64 = 4000;
pub const CLIMB_RATE: f32 = 2.0;
const CLIMB_DELTA_M: f32 = 20.0;
const CLIMB_MS: u64 = 12_000;
const MANUAL_MS: u64 = 50;

/// Box legs in body frame: (forward, right).
const LEGS: [(f32, f32); 4] = [(1.0, 0.0), (-1.0, 0.0), (0.0, -1.0), (0.0, 1.0)];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    TakeOff,
    Hover { since: u64 },
    Leg { i: usize, since: u64 },
    Brake { i: usize, since: u64 },
    Land,
    PlanePosition,
    Bank { i: usize, since: u64 },
    /// Climb step `i` (0 up, 1 down); `sent` once its target is commanded.
    Climb { i: usize, since: u64, sent: bool },
    ToMc(Transition),
    Decide,
    Reverting,
}

pub struct Verify {
    core: StepCore,
    hop: Hop,
    to: ToLoiter,
    phase: Phase,
    hover: Acc,
    box_acc: Acc,
    plane_acc: Acc,
    peaks: [f32; 4],
    leg_yaw: f32,
    last_manual: u64,
    result: Map<String, Value>,
}

impl Default for Verify {
    fn default() -> Self {
        Verify {
            core: StepCore::default(),
            hop: Hop::default(),
            to: ToLoiter::default(),
            phase: Phase::Idle,
            hover: Acc::default(),
            box_acc: Acc::default(),
            plane_acc: Acc::default(),
            peaks: [0.0; 4],
            leg_yaw: 0.0,
            last_manual: 0,
            result: Map::new(),
        }
    }
}

/// Per-axis comparison: `(tuned, baseline, worse_by_more_than_20%)`.
pub fn compare(tuned: [f32; 2], base: [f32; 2]) -> [(f32, f32, bool); 2] {
    let c = |t: f32, b: f32| (t, b, b > 0.0 && t > b * (1.0 + WORSE));
    [c(tuned[0], base[0]), c(tuned[1], base[1])]
}

fn plane_part(cx: &Cx) -> bool {
    cx.adapter.fw == Fw::ArduPilot && matches!(cx.adapter.class, Class::Fixedwing | Class::Vtol)
}

fn copter_part(cx: &Cx) -> bool {
    cx.adapter.copter_like() || cx.adapter.fw == Fw::Px4
}

impl Verify {
    fn start_copter(&mut self, cx: &mut Cx) {
        self.hop.start(cx, VERIFY_ALT_M);
        self.phase = Phase::TakeOff;
    }

    fn after_plane(&mut self, cx: &mut Cx) {
        let err = self.plane_acc.att_err_rms();
        self.result.insert("plane_track_err_rms_deg".into(), json!(err));
        if let (Some(e), Some(b)) = (err, cx.data.baseline_track_err_deg) {
            let c = compare(e, b);
            self.result.insert("plane_worse".into(), json!([c[0].2, c[1].2]));
        }
        if cx.adapter.class == Class::Vtol {
            self.phase = Phase::ToMc(Transition::to_mc(cx));
        } else {
            plane_loiter(cx, PLANE_LOITER_ALT_M);
            self.phase = Phase::Decide;
        }
    }

    fn decide(&mut self, cx: &mut Cx) {
        let worse = self
            .result
            .get("copter_worse")
            .into_iter()
            .chain(self.result.get("plane_worse"))
            .filter_map(Value::as_array)
            .flatten()
            .any(|v| v.as_bool() == Some(true));
        self.result.insert("worse".into(), json!(worse));
        if worse && cx.snapshot.is_some() {
            self.core.phase_key = Some("worse".into());
            self.core.result = Some(self.result.clone());
            self.core.wait_user(vec![Action::danger("revert_gains"), Action::secondary("keep_gains")]);
        } else {
            self.core.done(std::mem::take(&mut self.result));
        }
        self.phase = Phase::Idle;
    }

    fn manual(&mut self, cx: &mut Cx, x: f32, y: f32, z: f32) {
        if cx.now.saturating_sub(self.last_manual) >= MANUAL_MS {
            self.last_manual = cx.now;
            cx.out.manual(x, y, z, 0.0);
        }
    }
}

impl Step for Verify {
    step_core!("verify");

    fn flight(&self) -> bool {
        true
    }

    fn begin(&mut self, cx: &mut Cx) {
        *self = Verify::default();
        self.core.body_variant = match cx.adapter.class {
            Class::Fixedwing => Some("plane"),
            Class::Vtol => Some("vtol"),
            Class::Multirotor => None,
        };
        self.core.wait_user(vec![Action::primary("start"), Action::danger("abort")]);
    }

    fn tick(&mut self, cx: &mut Cx) {
        if !matches!(self.hop.leg, Leg::Idle) {
            self.hop.tick(cx);
            if let Leg::Failed(w) = self.hop.leg {
                self.hop.leg = Leg::Idle;
                self.phase = Phase::Idle;
                self.core.fail(&format!("setup.blocker.{w}"));
                return;
            }
        }
        match self.phase {
            Phase::Idle => {}
            Phase::TakeOff => {
                if self.hop.leg == Leg::Holding {
                    self.phase = Phase::Hover { since: cx.now };
                }
            }
            Phase::Hover { since } => {
                self.hover.sample(cx.fc, cx.now);
                if cx.now.saturating_sub(since) >= HOVER_MS {
                    self.leg_yaw = cx.fc.attitude.map(|(a, _)| a[2]).unwrap_or(0.0);
                    self.hop.leg = Leg::Idle;
                    self.phase = Phase::Leg { i: 0, since: cx.now };
                }
            }
            Phase::Leg { i, since } => {
                let (f, r) = LEGS[i];
                let (s, c) = self.leg_yaw.sin_cos();
                let vn = BOX_SPEED * (f * c - r * s);
                let ve = BOX_SPEED * (f * s + r * c);
                cx.out.velocity(vn, ve, 0.0, self.leg_yaw);
                self.box_acc.sample(cx.fc, cx.now);
                if let Some((l, _)) = cx.fc.local {
                    let along = (l[3] * vn + l[4] * ve) / BOX_SPEED;
                    self.peaks[i] = self.peaks[i].max(along);
                }
                self.core.progress = Some(0.2 + 0.15 * i as f32);
                if cx.now.saturating_sub(since) >= BOX_LEG_MS {
                    self.phase = Phase::Brake { i, since: cx.now };
                }
            }
            Phase::Brake { i, since } => {
                cx.out.velocity(0.0, 0.0, 0.0, self.leg_yaw);
                self.box_acc.sample(cx.fc, cx.now);
                if let Some((l, _)) = cx.fc.local {
                    let (f, r) = LEGS[i];
                    let (s, c) = self.leg_yaw.sin_cos();
                    let along = l[3] * (f * c - r * s) + l[4] * (f * s + r * c);
                    self.peaks[i] = self.peaks[i].max(along);
                }
                if cx.now.saturating_sub(since) >= BOX_BRAKE_MS {
                    if i + 1 < LEGS.len() {
                        self.phase = Phase::Leg { i: i + 1, since: cx.now };
                    } else {
                        let hover = self.hover.att_err_rms();
                        let boxe = self.box_acc.att_err_rms();
                        let os: Vec<f32> = self.peaks.iter().map(|p| (overshoot(*p, BOX_SPEED) * 100.0).round() / 100.0).collect();
                        self.result.insert("hover_att_err_rms_deg".into(), json!(hover));
                        self.result.insert("box_att_err_rms_deg".into(), json!(boxe));
                        self.result.insert("box_overshoot".into(), json!(os));
                        if let (Some(h), Some(b)) = (hover, cx.data.baseline_att_err_deg) {
                            let c = compare(h, b);
                            self.result.insert("copter_worse".into(), json!([c[0].2, c[1].2]));
                            self.result.insert("baseline_att_err_rms_deg".into(), json!(b));
                        }
                        self.hop.land(cx);
                        self.phase = Phase::Land;
                    }
                }
            }
            Phase::Land => {
                if self.hop.leg == Leg::Landed {
                    self.hop.leg = Leg::Idle;
                    self.phase = Phase::Decide;
                }
            }
            Phase::PlanePosition => {
                self.to.tick(cx);
                if let Some(w) = self.to.failed {
                    self.phase = Phase::Idle;
                    self.core.fail(&format!("setup.blocker.{w}"));
                } else if self.to.ready {
                    flight::set_mode(cx, Mode::Fbwa);
                    self.phase = Phase::Bank { i: 0, since: cx.now };
                }
            }
            Phase::Bank { i, since } => {
                let lim = cx.params.get("ROLL_LIMIT_DEG").or_else(|| cx.params.get("LIM_ROLL_CD").map(|v| v / 100.0)).unwrap_or(45.0);
                let sign = match i {
                    0 => 1.0,
                    1 => -1.0,
                    _ => 0.0,
                };
                let thr = cx.data.cruise_throttle.or_else(|| cx.params.get("TRIM_THROTTLE")).unwrap_or(45.0) / 100.0;
                self.manual(cx, 0.0, (sign * BANK_DEG / lim).clamp(-1.0, 1.0), thr);
                self.plane_acc.sample(cx.fc, cx.now);
                self.core.progress = Some(0.1 * i as f32);
                if cx.now.saturating_sub(since) >= BANK_MS {
                    if i < 2 {
                        self.phase = Phase::Bank { i: i + 1, since: cx.now };
                    } else {
                        plane_loiter(cx, PLANE_LOITER_ALT_M);
                        self.phase = Phase::Climb { i: 0, since: cx.now, sent: false };
                    }
                }
            }
            Phase::Climb { i, since, sent } => {
                if !sent {
                    // Command the step once the GUIDED loiter is back.
                    if flight::in_mode(cx, Mode::Guided) {
                        let h = height(cx).unwrap_or(PLANE_LOITER_ALT_M);
                        let target = if i == 0 { h + CLIMB_DELTA_M } else { h - CLIMB_DELTA_M };
                        flight::change_alt(cx, target, CLIMB_RATE);
                        self.phase = Phase::Climb { i, since: cx.now, sent: true };
                    } else if cx.now.saturating_sub(since) > 10_000 {
                        self.phase = Phase::Idle;
                        self.core.fail("setup.blocker.loiter_timeout");
                    }
                    return;
                }
                self.plane_acc.sample(cx.fc, cx.now);
                if cx.now.saturating_sub(since) >= CLIMB_MS {
                    if i == 0 {
                        self.phase = Phase::Climb { i: 1, since: cx.now, sent: false };
                    } else {
                        self.after_plane(cx);
                    }
                }
            }
            Phase::ToMc(mut t) => {
                t.tick(cx);
                self.phase = Phase::ToMc(t);
                match t {
                    Transition::Done => self.start_copter(cx),
                    Transition::Failed => {
                        self.phase = Phase::Idle;
                        self.core.fail("setup.blocker.transition_failed");
                    }
                    _ => {}
                }
            }
            Phase::Decide => self.decide(cx),
            Phase::Reverting => match writes_settled(cx) {
                None => {}
                Some(Ok(())) => {
                    self.phase = Phase::Idle;
                    self.result.insert("reverted".into(), json!(true));
                    self.core.done(std::mem::take(&mut self.result));
                }
                Some(Err(_)) => {
                    self.phase = Phase::Idle;
                    self.core.fail("setup.blocker.param_write_failed");
                }
            },
        }
    }

    fn action(&mut self, id: &str, _args: &Map<String, Value>, cx: &mut Cx) -> Result<(), ActErr> {
        match id {
            "start" | "retry" => {
                *self = Verify { core: std::mem::take(&mut self.core), ..Verify::default() };
                self.core.blockers.clear();
                self.core.phase_key = None;
                self.core.running();
                self.core.actions = vec![Action::danger("abort")];
                if plane_part(cx) {
                    self.to.start(cx);
                    self.phase = Phase::PlanePosition;
                } else if copter_part(cx) {
                    self.start_copter(cx);
                }
                Ok(())
            }
            "revert_gains" => {
                if cx.fc.armed {
                    return Err("armed");
                }
                let body = cx.snapshot.ok_or("no_snapshot")?;
                let adapter = cx.adapter;
                let mut n = 0;
                for (name, v) in parse_param_file(body) {
                    if adapter.is_gain_param(&name) && cx.params.has(&name) {
                        cx.params.set(&name, v);
                        n += 1;
                    }
                }
                self.result.insert("reverted_params".into(), json!(n));
                self.phase = Phase::Reverting;
                self.core.running();
                Ok(())
            }
            "keep_gains" => {
                self.result.insert("reverted".into(), json!(false));
                self.core.done(std::mem::take(&mut self.result));
                Ok(())
            }
            _ => Err("unknown_action"),
        }
    }

    fn abort(&mut self, _cx: &mut Cx) {
        self.phase = Phase::Idle;
        self.hop.leg = Leg::Idle;
    }
}
