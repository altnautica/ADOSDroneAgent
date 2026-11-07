//! Fixed-wing air steps: `first_flight`, `trim_air`, `airspeed_cal` and
//! `tecs_measure`. They chain in the air: each starts from a GUIDED loiter
//! over the tuning box and ends in one. A VTOL takes off vertically and
//! transitions to forward flight first.

use serde_json::{json, Map, Value};

use crate::setup::adapters::{ardupilot_plane, Mode};
use crate::setup::fcstate::Class;
use crate::setup::flight::{self, airborne, height, plane_loiter, Hop, Leg, Transition, PLANE_LOITER_ALT_M};
use crate::setup::measure::Acc;
use crate::setup::schema::{obj, Action};
use crate::setup::step::{writes_settled, ActErr, Cx, Step, StepCore};

/// First-flight loiter time.
pub const FIRST_FLIGHT_MS: u64 = 60_000;
pub const TRIM_MS: u64 = 30_000;
pub const AIRSPEED_CAL_MS: u64 = 120_000;
/// TECS measurement window per phase.
pub const TECS_PHASE_MS: u64 = 15_000;
/// Settle time before sampling a TECS phase.
const TECS_SETTLE_MS: u64 = 3000;
/// A TECS descent stops at this height.
const TECS_FLOOR_M: f32 = 50.0;
/// Measured TECS limits are scaled by this.
pub const TECS_MARGIN: f32 = 0.8;
const VTOL_CLIMB_M: f32 = 30.0;

/// Gets the aircraft to a fixed-wing loiter over the box: takes off if on
/// the ground (TAKEOFF mode; VTOL: vertical take-off then transition), or
/// re-centres the loiter if already flying.
#[derive(Debug, Clone)]
pub struct ToLoiter {
    hop: Hop,
    trans: Transition,
    sent_ms: u64,
    pub ready: bool,
    pub failed: Option<&'static str>,
}

impl Default for ToLoiter {
    fn default() -> Self {
        ToLoiter { hop: Hop::default(), trans: Transition::Idle, sent_ms: 0, ready: false, failed: None }
    }
}

fn fixedwing_flight(cx: &Cx) -> bool {
    cx.adapter.class == Class::Fixedwing
        || cx.fc.vtol_state == Some(ados_protocol::mavlink::ardupilotmega::MavVtolState::MAV_VTOL_STATE_FW)
}

impl ToLoiter {
    pub fn start(&mut self, cx: &mut Cx) {
        *self = ToLoiter::default();
        if airborne(cx) {
            if fixedwing_flight(cx) {
                plane_loiter(cx, PLANE_LOITER_ALT_M);
                self.sent_ms = cx.now;
            } else {
                self.trans = Transition::to_fw(cx);
            }
        } else {
            let alt = if cx.adapter.class == Class::Vtol {
                VTOL_CLIMB_M
            } else {
                cx.params.get("TKOFF_ALT").filter(|a| *a > 5.0).unwrap_or(50.0)
            };
            self.hop.start(cx, alt);
        }
    }

    pub fn tick(&mut self, cx: &mut Cx) {
        if self.ready || self.failed.is_some() {
            return;
        }
        if !matches!(self.hop.leg, Leg::Idle) {
            self.hop.tick(cx);
            match self.hop.leg {
                Leg::Failed(w) => self.failed = Some(w),
                Leg::Holding => {
                    self.hop.leg = Leg::Idle;
                    if cx.adapter.class == Class::Vtol {
                        self.trans = Transition::to_fw(cx);
                    } else {
                        plane_loiter(cx, PLANE_LOITER_ALT_M);
                        self.sent_ms = cx.now;
                    }
                }
                _ => {}
            }
            return;
        }
        match self.trans {
            Transition::ToFw { .. } | Transition::ToMc { .. } => {
                self.trans.tick(cx);
                return;
            }
            Transition::Failed => {
                self.failed = Some("transition_failed");
                return;
            }
            Transition::Done => {
                self.trans = Transition::Idle;
                plane_loiter(cx, PLANE_LOITER_ALT_M);
                self.sent_ms = cx.now;
            }
            Transition::Idle => {}
        }
        if self.sent_ms == 0 {
            return;
        }
        let near = height(cx).is_some_and(|h| (h - PLANE_LOITER_ALT_M).abs() < 10.0);
        if near && flight::in_mode(cx, Mode::Guided) {
            self.ready = true;
        } else if cx.now.saturating_sub(self.sent_ms) > 5000 && !flight::in_mode(cx, Mode::Guided) {
            plane_loiter(cx, PLANE_LOITER_ALT_M);
            self.sent_ms = cx.now;
        } else if cx.now.saturating_sub(self.sent_ms) > 120_000 {
            self.failed = Some("loiter_timeout");
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    FirstFlight,
    TrimAir,
    AirspeedCal,
    TecsMeasure,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    Positioning,
    Measuring { since: u64 },
    Tecs { stage: u8, since: u64 },
    ReadBack,
}

pub struct PlaneAir {
    core: StepCore,
    kind: Kind,
    to: ToLoiter,
    phase: Phase,
    acc: Acc,
    result: Map<String, Value>,
    /// TECS stage samples (m/s, positive up).
    climb: Vec<f32>,
    restore: Vec<(String, f32)>,
}

impl PlaneAir {
    pub fn new(kind: Kind) -> Self {
        PlaneAir {
            core: StepCore::default(),
            kind,
            to: ToLoiter::default(),
            phase: Phase::Idle,
            acc: Acc::default(),
            result: Map::new(),
            climb: Vec::new(),
            restore: Vec::new(),
        }
    }

    fn duration(&self) -> u64 {
        match self.kind {
            Kind::FirstFlight => FIRST_FLIGHT_MS,
            Kind::TrimAir => TRIM_MS,
            Kind::AirspeedCal => AIRSPEED_CAL_MS,
            Kind::TecsMeasure => TECS_PHASE_MS * 3,
        }
    }

    fn surface_channels(cx: &Cx) -> Vec<u8> {
        (1..=16u8)
            .filter(|k| {
                cx.params
                    .get(&format!("SERVO{k}_FUNCTION"))
                    .is_some_and(|f| ardupilot_plane::surface_test(f as u8).is_some())
            })
            .collect()
    }

    fn begin_measure(&mut self, cx: &mut Cx) {
        match self.kind {
            Kind::TrimAir => cx.params.set("SERVO_AUTO_TRIM", 1.0),
            Kind::AirspeedCal => cx.params.set("ARSPD_AUTOCAL", 1.0),
            Kind::TecsMeasure => {
                self.start_tecs_stage(cx, 0);
                return;
            }
            Kind::FirstFlight => {}
        }
        self.phase = Phase::Measuring { since: cx.now };
    }

    /// TECS stages: 0 full-throttle climb, 1 idle glide at cruise, 2 idle
    /// dive at maximum airspeed. The FC limits are raised for the stage so
    /// TECS saturates (throttle at max, or at zero), and restored after.
    fn start_tecs_stage(&mut self, cx: &mut Cx, stage: u8) {
        let h = height(cx).unwrap_or(PLANE_LOITER_ALT_M);
        self.climb.clear();
        match stage {
            0 => {
                if let Some(old) = cx.params.get("TECS_CLMB_MAX") {
                    self.restore.push(("TECS_CLMB_MAX".into(), old));
                    cx.params.set("TECS_CLMB_MAX", 20.0);
                }
                flight::change_alt(cx, h + 150.0, 0.0);
            }
            1 | 2 => {
                for (n, v) in [("TECS_SINK_MAX", 20.0f32), ("THR_MAX", 0.0)] {
                    if !self.restore.iter().any(|(r, _)| r == n) {
                        if let Some(old) = cx.params.get(n) {
                            self.restore.push((n.into(), old));
                        }
                    }
                    cx.params.set(n, v);
                }
                if stage == 2 {
                    if let Some(n) = cx.params.first_present(ardupilot_plane::AIRSPEED_MAX) {
                        if let Some(max) = cx.params.get(n) {
                            flight::change_speed(cx, max);
                        }
                    }
                }
                flight::change_alt(cx, (h - 150.0).max(TECS_FLOOR_M - 20.0), 0.0);
            }
            _ => {}
        }
        self.phase = Phase::Tecs { stage, since: cx.now };
    }

    fn end_tecs(&mut self, cx: &mut Cx) {
        for (n, v) in self.restore.drain(..) {
            cx.params.set(&n, v);
        }
        if let Some(n) = cx.params.first_present(ardupilot_plane::AIRSPEED_CRUISE) {
            let cruise = cx.params.get(n).map(|c| if n == "TRIM_ARSPD_CM" { c / 100.0 } else { c });
            if let Some(c) = cruise {
                flight::change_speed(cx, c);
            }
        }
        plane_loiter(cx, PLANE_LOITER_ALT_M);
    }

    fn finish(&mut self, cx: &mut Cx) {
        match self.kind {
            Kind::FirstFlight => {
                let err = self.acc.att_err_rms();
                cx.data.baseline_track_err_deg = err;
                let thr = self.acc.throttle_mean();
                cx.data.cruise_throttle = thr;
                if let Some(t) = thr {
                    cx.params.set("TRIM_THROTTLE", t.round());
                }
                self.result = obj([
                    ("trim_throttle", json!(thr.map(|t| t.round()))),
                    ("track_err_rms_deg", json!(err)),
                    ("airspeed_excess_mps", json!(self.acc.airspeed_excess_mean())),
                    ("vibration_max", json!(self.acc.vibe_max)),
                ]);
                self.phase = Phase::ReadBack;
            }
            Kind::TrimAir => {
                for k in Self::surface_channels(cx) {
                    cx.params.read(&format!("SERVO{k}_TRIM"));
                }
                self.phase = Phase::ReadBack;
            }
            Kind::AirspeedCal => {
                cx.params.set("ARSPD_AUTOCAL", 0.0);
                cx.params.read("ARSPD_RATIO");
                self.phase = Phase::ReadBack;
            }
            Kind::TecsMeasure => {
                self.end_tecs(cx);
                self.phase = Phase::ReadBack;
            }
        }
    }

    fn read_back(&mut self, cx: &mut Cx) {
        match self.kind {
            Kind::FirstFlight => {}
            Kind::TrimAir => {
                let trims: Map<String, Value> = Self::surface_channels(cx)
                    .into_iter()
                    .filter_map(|k| cx.params.get(&format!("SERVO{k}_TRIM")).map(|v| (format!("SERVO{k}_TRIM"), json!(v))))
                    .collect();
                self.result = obj([("trims", Value::Object(trims))]);
            }
            Kind::AirspeedCal => {
                self.result = obj([("arspd_ratio", json!(cx.params.get("ARSPD_RATIO")))]);
            }
            Kind::TecsMeasure => {}
        }
        self.phase = Phase::Idle;
        self.core.done(std::mem::take(&mut self.result));
    }
}

fn mean(v: &[f32]) -> Option<f32> {
    (!v.is_empty()).then(|| v.iter().sum::<f32>() / v.len() as f32)
}

impl Step for PlaneAir {
    fn id(&self) -> &'static str {
        match self.kind {
            Kind::FirstFlight => "first_flight",
            Kind::TrimAir => "trim_air",
            Kind::AirspeedCal => "airspeed_cal",
            Kind::TecsMeasure => "tecs_measure",
        }
    }

    fn core(&self) -> &StepCore {
        &self.core
    }

    fn core_mut(&mut self) -> &mut StepCore {
        &mut self.core
    }

    fn flight(&self) -> bool {
        true
    }

    fn optional(&self) -> bool {
        self.kind != Kind::FirstFlight
    }

    fn skip_reason(&self, cx: &Cx) -> Option<&'static str> {
        match self.kind {
            Kind::AirspeedCal if cx.params.get("ARSPD_TYPE").unwrap_or(0.0) == 0.0 => Some("no_airspeed_sensor"),
            Kind::AirspeedCal if !cx.params.has("ARSPD_AUTOCAL") => Some("not_applicable"),
            _ => None,
        }
    }

    fn begin(&mut self, cx: &mut Cx) {
        *self = PlaneAir::new(self.kind);
        self.core.body_variant = (cx.adapter.class == Class::Vtol).then_some("vtol");
        self.core.wait_user(vec![Action::primary("start"), Action::danger("abort")]);
    }

    fn tick(&mut self, cx: &mut Cx) {
        match self.phase {
            Phase::Idle => {}
            Phase::Positioning => {
                self.to.tick(cx);
                if let Some(w) = self.to.failed {
                    self.phase = Phase::Idle;
                    self.core.fail(&format!("setup.blocker.{w}"));
                } else if self.to.ready {
                    if self.kind == Kind::FirstFlight {
                        if let Some((_, s, _)) = cx.fc.wind {
                            if s > super::first_hover::MAX_START_WIND {
                                self.phase = Phase::Idle;
                                self.core.result = Some(obj([("wind_mps", json!(s))]));
                                self.core.fail("setup.blocker.wind");
                                return;
                            }
                        } else {
                            self.core.set_live("wind", "unavailable");
                        }
                    }
                    self.begin_measure(cx);
                }
            }
            Phase::Measuring { since } => {
                self.acc.sample(cx.fc, cx.now);
                let el = cx.now.saturating_sub(since);
                self.core.progress = Some((el as f32 / self.duration() as f32).min(1.0));
                if let Some(t) = self.acc.throttle_mean() {
                    self.core.set_live("throttle_pct", t.round());
                }
                if el >= self.duration() {
                    self.finish(cx);
                }
            }
            Phase::Tecs { stage, since } => {
                let el = cx.now.saturating_sub(since);
                if el > TECS_SETTLE_MS {
                    if let Some(c) = cx.fc.climb {
                        self.climb.push(c);
                    }
                }
                self.core.progress = Some((stage as f32 + (el as f32 / TECS_PHASE_MS as f32).min(1.0)) / 3.0);
                let low = stage > 0 && height(cx).is_some_and(|h| h < TECS_FLOOR_M);
                if el >= TECS_PHASE_MS || (low && el > TECS_SETTLE_MS + 5000) {
                    let m = mean(&self.climb);
                    match stage {
                        0 => {
                            if let Some(m) = m.filter(|m| *m > 0.5) {
                                let v = (TECS_MARGIN * m * 10.0).round() / 10.0;
                                self.result.insert("climb_measured_mps".into(), json!(m));
                                self.result.insert("TECS_CLMB_MAX".into(), json!(v));
                                self.restore.retain(|(n, _)| n != "TECS_CLMB_MAX");
                                cx.params.set("TECS_CLMB_MAX", v);
                            }
                            // Climb done: back to the loiter height before gliding.
                            self.start_tecs_stage(cx, 1);
                        }
                        1 => {
                            if let Some(m) = m.filter(|m| *m < -0.2) {
                                let v = ((-m) * 10.0).round() / 10.0;
                                self.result.insert("TECS_SINK_MIN".into(), json!(v));
                                cx.params.set("TECS_SINK_MIN", v);
                            }
                            self.start_tecs_stage(cx, 2);
                        }
                        _ => {
                            if let Some(m) = m.filter(|m| *m < -0.5) {
                                let v = (TECS_MARGIN * (-m) * 10.0).round() / 10.0;
                                self.result.insert("sink_max_measured_mps".into(), json!(-m));
                                self.result.insert("TECS_SINK_MAX".into(), json!(v));
                                self.restore.retain(|(n, _)| n != "TECS_SINK_MAX");
                                cx.params.set("TECS_SINK_MAX", v);
                            }
                            self.finish(cx);
                        }
                    }
                }
            }
            Phase::ReadBack => match writes_settled(cx) {
                None => {}
                Some(Err(_)) => {
                    self.phase = Phase::Idle;
                    self.core.fail("setup.blocker.param_write_failed");
                }
                Some(Ok(())) => self.read_back(cx),
            },
        }
    }

    fn action(&mut self, id: &str, _args: &Map<String, Value>, cx: &mut Cx) -> Result<(), ActErr> {
        match id {
            "start" | "retry" => {
                self.acc = Acc::default();
                self.result.clear();
                self.to.start(cx);
                self.phase = Phase::Positioning;
                self.core.blockers.clear();
                self.core.running();
                self.core.actions = vec![Action::danger("abort")];
                Ok(())
            }
            _ => Err("unknown_action"),
        }
    }

    fn abort(&mut self, cx: &mut Cx) {
        for (n, v) in self.restore.drain(..) {
            cx.params.set(&n, v);
        }
        if self.kind == Kind::AirspeedCal && matches!(self.phase, Phase::Measuring { .. }) {
            cx.params.set("ARSPD_AUTOCAL", 0.0);
        }
        self.phase = Phase::Idle;
    }
}
