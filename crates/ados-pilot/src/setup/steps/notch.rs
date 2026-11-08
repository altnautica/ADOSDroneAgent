//! `notch_static` (ground, needs ESC telemetry) and `notch_air` (in flight,
//! throttle-based, from the learned hover throttle): the harmonic notch.

use serde_json::{json, Map, Value};

use crate::setup::adapters::ardupilot_copter::rotor_hz;
use crate::setup::fcstate::Fw;
use crate::setup::schema::{obj, Action};
use crate::setup::step::{writes_settled, ActErr, Cx, Reboot, Rebooted, Step, StepCore};

/// Notch reference when no learned hover throttle exists.
pub const DEFAULT_HOVER_THROTTLE: f32 = 0.35;
/// Notch centre as a fraction of the hover rotor frequency.
pub const FREQ_FACTOR: f32 = 0.8;
/// `INS_HNTCH_FREQ` range.
const FREQ_MIN: f32 = 10.0;
const FREQ_MAX: f32 = 495.0;
/// `INS_HNTCH_MODE`: 1 throttle, 3 ESC telemetry.
const MODE_THROTTLE: f32 = 1.0;
const MODE_ESC: f32 = 3.0;

/// Notch frequency (Hz) for a hover throttle.
pub fn notch_freq(kv: f32, cells: f32, hover: f32) -> f32 {
    (FREQ_FACTOR * rotor_hz(kv, cells, hover)).clamp(FREQ_MIN, FREQ_MAX)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    Enabling,
    Rebooting,
    Writing,
}

pub struct Notch {
    core: StepCore,
    air: bool,
    phase: Phase,
    reboot: Reboot,
    result: Map<String, Value>,
}

impl Notch {
    pub fn new(air: bool) -> Self {
        Notch {
            core: StepCore::default(),
            air,
            phase: Phase::Idle,
            reboot: Reboot::default(),
            result: Map::new(),
        }
    }

    fn hover(&self, cx: &Cx) -> f32 {
        if self.air {
            cx.data
                .hover_throttle
                .or_else(|| cx.params.get(&format!("{}_THST_HOVER", cx.adapter.mot())))
                .unwrap_or(DEFAULT_HOVER_THROTTLE)
        } else {
            DEFAULT_HOVER_THROTTLE
        }
    }

    fn write_rest(&mut self, cx: &mut Cx) {
        let kv = cx.data.motor_kv.unwrap_or(0.0);
        let cells = cx.data.cells.unwrap_or(0.0);
        let hover = self.hover(cx);
        let freq = notch_freq(kv, cells, hover);
        let mode = if self.air { MODE_THROTTLE } else { MODE_ESC };
        cx.params.set("INS_HNTCH_MODE", mode);
        cx.params
            .set("INS_HNTCH_REF", if self.air { hover } else { 1.0 });
        cx.params.set("INS_HNTCH_FREQ", freq.round());
        cx.params.set("INS_HNTCH_BW", (freq / 2.0).round());
        self.result = obj([
            ("mode", json!(mode)),
            ("ref", json!(if self.air { hover } else { 1.0 })),
            ("freq_hz", json!(freq.round())),
            ("bw_hz", json!((freq / 2.0).round())),
            ("hover_throttle", json!(hover)),
        ]);
        self.phase = Phase::Writing;
    }
}

impl Step for Notch {
    fn id(&self) -> &'static str {
        if self.air {
            "notch_air"
        } else {
            "notch_static"
        }
    }

    fn core(&self) -> &StepCore {
        &self.core
    }

    fn core_mut(&mut self) -> &mut StepCore {
        &mut self.core
    }

    fn on_skipped(&mut self, reason: &str, cx: &mut Cx) {
        if reason == "deferred_no_esc_telemetry" {
            cx.data.notch_deferred = true;
        }
    }

    fn optional(&self) -> bool {
        true
    }

    fn skip_reason(&self, cx: &Cx) -> Option<&'static str> {
        if cx.adapter.fw != Fw::ArduPilot || !cx.adapter.copter_like() {
            return Some("not_applicable");
        }
        if self.air {
            (!cx.data.notch_deferred).then_some("notch_done_on_ground")
        } else if !cx.fc.esc_telemetry(cx.now) {
            Some("deferred_no_esc_telemetry")
        } else {
            None
        }
    }

    fn begin(&mut self, cx: &mut Cx) {
        self.core = StepCore::default();
        self.phase = Phase::Idle;
        if !self.air && !cx.fc.esc_telemetry(cx.now) {
            cx.data.notch_deferred = true;
        }
        if cx.data.motor_kv.is_none() || cx.data.cells.is_none() {
            self.core.fail("setup.blocker.frame_first");
            return;
        }
        let hover = self.hover(cx);
        self.core.args.insert(
            "freq_hz".into(),
            json!(notch_freq(
                cx.data.motor_kv.unwrap_or(0.0),
                cx.data.cells.unwrap_or(0.0),
                hover
            )
            .round()),
        );
        self.core
            .wait_user(vec![Action::primary("apply"), Action::secondary("skip")]);
    }

    fn tick(&mut self, cx: &mut Cx) {
        match self.phase {
            Phase::Idle => {}
            Phase::Enabling => match writes_settled(cx) {
                None => {}
                Some(Err(_)) => {
                    self.phase = Phase::Idle;
                    self.core.fail("setup.blocker.param_write_failed");
                }
                Some(Ok(())) => {
                    // The other INS_HNTCH_* parameters appear once enabled and rebooted.
                    self.phase = Phase::Rebooting;
                    self.core.phase_key = Some("rebooting".into());
                    self.reboot.start(cx);
                }
            },
            Phase::Rebooting => match self.reboot.tick(cx) {
                Rebooted::Waiting => {}
                Rebooted::Back => {
                    self.core.phase_key = None;
                    self.write_rest(cx);
                }
                Rebooted::Lost => {
                    self.phase = Phase::Idle;
                    self.core.phase_key = None;
                    self.core.fail("setup.blocker.timeout");
                }
            },
            Phase::Writing => match writes_settled(cx) {
                None => {}
                Some(Err(_)) => {
                    self.phase = Phase::Idle;
                    self.core.fail("setup.blocker.param_write_failed");
                }
                Some(Ok(())) => {
                    self.phase = Phase::Idle;
                    if self.air {
                        cx.data.notch_deferred = false;
                    }
                    self.core.done(std::mem::take(&mut self.result));
                }
            },
        }
    }

    fn action(&mut self, id: &str, _args: &Map<String, Value>, cx: &mut Cx) -> Result<(), ActErr> {
        match id {
            "apply" => {
                if cx.fc.armed {
                    return Err("armed");
                }
                if cx.params.get("INS_HNTCH_ENABLE").unwrap_or(0.0) == 1.0
                    && cx.params.has("INS_HNTCH_FREQ")
                {
                    self.write_rest(cx);
                } else {
                    cx.params.set("INS_HNTCH_ENABLE", 1.0);
                    self.phase = Phase::Enabling;
                }
                self.core.running();
                Ok(())
            }
            _ => Err("unknown_action"),
        }
    }
}
