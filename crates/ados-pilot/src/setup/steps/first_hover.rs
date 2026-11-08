//! `first_hover` (copter, VTOL): take off to 3 m, hover 45 s, land.
//! Measures vibration, the learned hover throttle, attitude tracking error
//! and rate oscillation (3–15 Hz; found → land at once). A VTOL then flies a
//! transition to forward flight and back before landing.

use serde_json::{json, Map, Value};

use crate::setup::flight::{height, plane_loiter, Hop, Leg, Transition};
use crate::setup::fcstate::{Class, Fw};
use crate::setup::measure::{band_rms, vibe_verdict, Acc, ATT_ERR_PASS_DEG, OSC_RMS_RAD_S};
use crate::setup::schema::{obj, Action};
use crate::setup::step::{step_core, ActErr, Cx, Step, StepCore};

pub const HOVER_ALT_M: f32 = 3.0;
pub const HOVER_MS: u64 = 45_000;
/// Wind above this after take-off ends the wizard.
pub const MAX_START_WIND: f32 = 6.0;
/// VTOL climbs here before transitioning.
const VTOL_TRANSITION_ALT_M: f32 = 30.0;
/// Time in forward flight before transitioning back.
const VTOL_FW_MS: u64 = 15_000;
/// Give up on the VTOL climb (and land) after this long.
const VTOL_CLIMB_TIMEOUT_MS: u64 = 60_000;
/// Re-send the climb target this often while it is not reached.
const VTOL_CLIMB_RESEND_MS: u64 = 5000;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    Flying,
    Hover { since: u64 },
    Climb { since: u64, sent: u64 },
    Fw(Transition),
    FwHold { since: u64 },
    Mc(Transition),
    Landing { reason: Option<&'static str> },
    ReadBack,
}

pub struct FirstHover {
    core: StepCore,
    hop: Hop,
    phase: Phase,
    acc: Acc,
    result: Map<String, Value>,
}

impl Default for FirstHover {
    fn default() -> Self {
        FirstHover { core: StepCore::default(), hop: Hop::default(), phase: Phase::Idle, acc: Acc::default(), result: Map::new() }
    }
}

impl FirstHover {
    fn land(&mut self, cx: &mut Cx, reason: Option<&'static str>) {
        self.hop.land(cx);
        self.phase = Phase::Landing { reason };
    }

    fn measure_result(&self, cx: &Cx) -> Map<String, Value> {
        let peak = self.acc.vibe_peak();
        let err = self.acc.att_err_rms();
        let mut r = obj([
            ("vibration_max", json!(self.acc.vibe_max)),
            ("vibration", json!(vibe_verdict(peak))),
            ("att_err_rms_deg", json!(err)),
            ("att_err_pass", json!(err.is_some_and(|e| e[0] < ATT_ERR_PASS_DEG && e[1] < ATT_ERR_PASS_DEG))),
        ]);
        if let Some((_, s, _)) = cx.fc.wind {
            r.insert("wind_mps".into(), json!(s));
        }
        r
    }
}

impl Step for FirstHover {
    step_core!("first_hover");

    fn flight(&self) -> bool {
        true
    }

    fn begin(&mut self, cx: &mut Cx) {
        *self = FirstHover::default();
        self.core.body_variant = (cx.adapter.class == Class::Vtol).then_some("vtol");
        self.core.wait_user(vec![Action::primary("start"), Action::danger("abort")]);
    }

    fn tick(&mut self, cx: &mut Cx) {
        self.hop.tick(cx);
        if let Leg::Failed(why) = self.hop.leg {
            if self.phase != Phase::Idle {
                self.phase = Phase::Idle;
                self.core.fail(&format!("setup.blocker.{why}"));
            }
            return;
        }
        match self.phase {
            Phase::Idle => {}
            Phase::Flying => {
                if self.hop.leg == Leg::Holding {
                    if let Some((_, s, _)) = cx.fc.wind {
                        if s > MAX_START_WIND {
                            self.result = obj([("wind_mps", json!(s))]);
                            self.land(cx, Some("wind"));
                            return;
                        }
                    } else {
                        self.core.set_live("wind", "unavailable");
                    }
                    self.phase = Phase::Hover { since: cx.now };
                }
            }
            Phase::Hover { since } => {
                self.acc.sample(cx.fc, cx.now);
                let el = cx.now.saturating_sub(since);
                self.core.progress = Some((el as f32 / HOVER_MS as f32).min(1.0) * 0.8);
                self.core.set_live("vibration_max", json!(self.acc.vibe_max));
                if let Some(e) = self.acc.att_err_rms() {
                    self.core.set_live("att_err_rms_deg", json!(e));
                }
                if el > 3000 {
                    if let Some(rms) = band_rms(&cx.fc.rates, cx.now) {
                        self.core.set_live("osc_rms", json!(rms));
                        if rms[0] > OSC_RMS_RAD_S || rms[1] > OSC_RMS_RAD_S {
                            let p = cx.adapter.atc();
                            let halve: Vec<Value> = [format!("{p}_RAT_RLL_P"), format!("{p}_RAT_PIT_P")]
                                .iter()
                                .filter_map(|n| cx.params.get(n).map(|v| json!({"name": n, "now": v, "suggest": v / 2.0})))
                                .collect();
                            self.result = obj([("oscillation_rms", json!(rms)), ("suggest", Value::Array(halve))]);
                            self.land(cx, Some("oscillation"));
                            return;
                        }
                    }
                }
                if el >= HOVER_MS {
                    self.result = self.measure_result(cx);
                    if cx.adapter.class == Class::Vtol && cx.adapter.fw == Fw::ArduPilot {
                        // ArduPlane takes no local position setpoints: the VTOL climbs to its
                        // GUIDED (Q_GUIDED_MODE 1, a VTOL hover) target height instead.
                        plane_loiter(cx, VTOL_TRANSITION_ALT_M);
                        self.phase = Phase::Climb { since: cx.now, sent: cx.now };
                    } else {
                        self.land(cx, None);
                    }
                }
            }
            Phase::Climb { since, sent } => {
                if height(cx).is_some_and(|h| h > VTOL_TRANSITION_ALT_M - 2.0) {
                    // The hold leg stops sending once fixed-wing flight starts.
                    self.hop.leg = Leg::Idle;
                    self.phase = Phase::Fw(Transition::to_fw(cx));
                } else if cx.now.saturating_sub(since) > VTOL_CLIMB_TIMEOUT_MS {
                    self.land(cx, Some("timeout"));
                } else if cx.now.saturating_sub(sent) > VTOL_CLIMB_RESEND_MS {
                    plane_loiter(cx, VTOL_TRANSITION_ALT_M);
                    self.phase = Phase::Climb { since, sent: cx.now };
                }
            }
            Phase::Fw(mut t) => {
                t.tick(cx);
                self.phase = match t {
                    Transition::Done => Phase::FwHold { since: cx.now },
                    Transition::Failed => {
                        self.result.insert("transition_fw".into(), json!(false));
                        Phase::Mc(Transition::to_mc(cx))
                    }
                    other => Phase::Fw(other),
                };
            }
            Phase::FwHold { since } => {
                if cx.now.saturating_sub(since) >= VTOL_FW_MS {
                    self.result.insert("transition_fw".into(), json!(true));
                    self.phase = Phase::Mc(Transition::to_mc(cx));
                }
            }
            Phase::Mc(mut t) => {
                t.tick(cx);
                self.phase = match t {
                    Transition::Done => {
                        self.result.insert("transition_mc".into(), json!(true));
                        self.hop.land(cx);
                        Phase::Landing { reason: None }
                    }
                    Transition::Failed => {
                        self.result.insert("transition_mc".into(), json!(false));
                        self.hop.land(cx);
                        Phase::Landing { reason: Some("transition_failed") }
                    }
                    other => Phase::Mc(other),
                };
            }
            Phase::Landing { reason } => {
                if self.hop.leg == Leg::Landed {
                    match reason {
                        Some(r) => {
                            self.phase = Phase::Idle;
                            self.core.result = Some(std::mem::take(&mut self.result));
                            self.core.fail(&format!("setup.blocker.{r}"));
                        }
                        None => {
                            cx.params.read(&format!("{}_THST_HOVER", cx.adapter.mot()));
                            self.phase = Phase::ReadBack;
                        }
                    }
                }
            }
            Phase::ReadBack => {
                if cx.params.idle() {
                    let name = format!("{}_THST_HOVER", cx.adapter.mot());
                    let hover = cx.params.get(&name);
                    cx.data.hover_throttle = hover;
                    cx.data.baseline_att_err_deg = self.acc.att_err_rms();
                    self.result.insert("hover_throttle".into(), json!(hover));
                    self.phase = Phase::Idle;
                    let vib = vibe_verdict(self.acc.vibe_peak());
                    let mut result = std::mem::take(&mut self.result);
                    if vib == "fail" {
                        self.core.result = Some(result);
                        self.core.fail("setup.blocker.vibration");
                    } else {
                        result.insert("vibration_verdict".into(), json!(vib));
                        self.core.done(result);
                    }
                }
            }
        }
    }

    fn action(&mut self, id: &str, _args: &Map<String, Value>, cx: &mut Cx) -> Result<(), ActErr> {
        match id {
            "start" => {
                self.acc = Acc::default();
                self.result.clear();
                self.hop.start(cx, HOVER_ALT_M);
                self.phase = Phase::Flying;
                self.core.blockers.clear();
                self.core.running();
                self.core.actions = vec![Action::danger("abort")];
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
