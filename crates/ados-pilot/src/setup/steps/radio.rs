//! `radio`: RC min/max/trim capture from RC_CHANNELS, and `rc_ados_switch`:
//! assigning one flight-mode switch position to GUIDED, labelled "ADOS".

use serde_json::{json, Map, Value};

use crate::setup::adapters::Mode;
use crate::setup::fcstate::Fw;
use crate::setup::schema::{obj, Action};
use crate::setup::step::{step_core, writes_settled, ActErr, Cx, Step, StepCore};

/// A channel whose captured range is narrower than this was not moved.
const MIN_TRAVEL_US: u16 = 300;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    Capturing,
    Writing,
}

#[derive(Default)]
pub struct Radio {
    core: StepCore,
    phase: Option<Phase>,
    min: Vec<u16>,
    max: Vec<u16>,
    trim: Vec<u16>,
    result: Map<String, Value>,
}

impl Radio {
    fn capture(&mut self, cx: &Cx) {
        let Some((ch, _)) = &cx.fc.rc else { return };
        if self.min.len() < ch.len() {
            self.min.resize(ch.len(), u16::MAX);
            self.max.resize(ch.len(), 0);
        }
        for (i, v) in ch.iter().enumerate() {
            if *v == 0 || *v == u16::MAX {
                continue;
            }
            self.min[i] = self.min[i].min(*v);
            self.max[i] = self.max[i].max(*v);
        }
    }

    fn moved(&self) -> Vec<usize> {
        (0..self.min.len())
            .filter(|i| self.max[*i] > self.min[*i] && self.max[*i] - self.min[*i] >= MIN_TRAVEL_US)
            .collect()
    }
}

impl Step for Radio {
    step_core!("radio");

    fn optional(&self) -> bool {
        true
    }

    fn skip_reason(&self, cx: &Cx) -> Option<&'static str> {
        (!cx.fc.rc_present(cx.now)).then_some("no_rc")
    }

    fn begin(&mut self, _cx: &mut Cx) {
        self.core = StepCore::default();
        self.phase = Some(Phase::Idle);
        self.min.clear();
        self.max.clear();
        self.trim.clear();
        self.core
            .wait_user(vec![Action::primary("start"), Action::secondary("skip")]);
    }

    fn tick(&mut self, cx: &mut Cx) {
        match self.phase {
            Some(Phase::Capturing) => {
                self.capture(cx);
                let live: Vec<Value> = (0..self.min.len())
                    .map(|i| {
                        let cur = cx.fc.rc_channel(i + 1).unwrap_or(0);
                        json!({"ch": i + 1, "pwm": cur, "min": if self.min[i] == u16::MAX { 0 } else { self.min[i] }, "max": self.max[i]})
                    })
                    .collect();
                self.core.set_live("channels", Value::Array(live));
                self.core.progress = Some((self.moved().len().min(4) as f32) / 4.0);
            }
            Some(Phase::Writing) => match writes_settled(cx) {
                None => {}
                Some(Ok(())) => {
                    self.phase = Some(Phase::Idle);
                    self.core.done(std::mem::take(&mut self.result));
                }
                Some(Err(_)) => {
                    self.phase = Some(Phase::Idle);
                    self.core.fail("setup.blocker.param_write_failed");
                }
            },
            _ => {}
        }
    }

    fn action(&mut self, id: &str, _args: &Map<String, Value>, cx: &mut Cx) -> Result<(), ActErr> {
        match id {
            "start" => {
                if !cx.fc.rc_present(cx.now) {
                    return Err("no_rc");
                }
                self.min.clear();
                self.max.clear();
                self.phase = Some(Phase::Capturing);
                self.core.running();
                self.core.phase_key = Some("move".into());
                self.core
                    .wait_user(vec![Action::primary("center"), Action::secondary("cancel")]);
                Ok(())
            }
            "center" => {
                if self.phase != Some(Phase::Capturing) {
                    return Err("not_capturing");
                }
                let moved = self.moved();
                if moved.len() < 4 {
                    return Err("sticks_not_moved");
                }
                let (ch, _) = cx.fc.rc.clone().ok_or("no_rc")?;
                self.trim = ch;
                let mut channels = Vec::new();
                for i in moved {
                    let n = i + 1;
                    let trim = self
                        .trim
                        .get(i)
                        .copied()
                        .unwrap_or(1500)
                        .clamp(self.min[i], self.max[i]);
                    cx.params.set(&format!("RC{n}_MIN"), self.min[i] as f32);
                    cx.params.set(&format!("RC{n}_MAX"), self.max[i] as f32);
                    cx.params.set(&format!("RC{n}_TRIM"), trim as f32);
                    channels.push(
                        json!({"ch": n, "min": self.min[i], "max": self.max[i], "trim": trim}),
                    );
                }
                self.result = obj([("channels", Value::Array(channels))]);
                self.phase = Some(Phase::Writing);
                self.core.phase_key = None;
                self.core.running();
                Ok(())
            }
            "cancel" => {
                self.abort(cx);
                Ok(())
            }
            _ => Err("unknown_action"),
        }
    }

    fn abort(&mut self, _cx: &mut Cx) {
        self.phase = Some(Phase::Idle);
        self.core.fail("setup.blocker.cancelled");
    }
}

/// ArduPilot flight-mode slot (1–6) for a mode-channel PWM, from
/// `RC_Channel::read_mode_switch` bands.
pub fn mode_slot(pwm: u16) -> u8 {
    match pwm {
        0..=1230 => 1,
        1231..=1360 => 2,
        1361..=1490 => 3,
        1491..=1620 => 4,
        1621..=1749 => 5,
        _ => 6,
    }
}

#[derive(Default)]
pub struct RcAdosSwitch {
    core: StepCore,
    writing: Option<u8>,
}

impl Step for RcAdosSwitch {
    step_core!("rc_ados_switch");

    fn optional(&self) -> bool {
        true
    }

    fn skip_reason(&self, cx: &Cx) -> Option<&'static str> {
        if cx.adapter.fw != Fw::ArduPilot {
            Some("not_applicable")
        } else if !cx.fc.rc_present(cx.now) {
            Some("no_rc")
        } else {
            None
        }
    }

    fn begin(&mut self, cx: &mut Cx) {
        self.core = StepCore::default();
        self.writing = None;
        let ch = cx.params.get("FLTMODE_CH").unwrap_or(5.0) as u8;
        self.core.args.insert("mode_channel".into(), json!(ch));
        self.core
            .wait_user(vec![Action::primary("capture"), Action::secondary("skip")]);
    }

    fn tick(&mut self, cx: &mut Cx) {
        let ch = cx.params.get("FLTMODE_CH").unwrap_or(5.0) as usize;
        if let Some(pwm) = cx.fc.rc_channel(ch) {
            self.core.set_live("pwm", pwm);
            self.core.set_live("position", mode_slot(pwm));
        }
        let Some(slot) = self.writing else { return };
        match writes_settled(cx) {
            None => {}
            Some(Ok(())) => {
                self.writing = None;
                cx.out
                    .effect(crate::setup::out::Effect::SetRcAdosPosition(slot));
                self.core
                    .done(obj([("position", json!(slot)), ("channel", json!(ch))]));
            }
            Some(Err(_)) => {
                self.writing = None;
                self.core.fail("setup.blocker.param_write_failed");
            }
        }
    }

    fn action(&mut self, id: &str, _args: &Map<String, Value>, cx: &mut Cx) -> Result<(), ActErr> {
        if id != "capture" {
            return Err("unknown_action");
        }
        let ch = cx.params.get("FLTMODE_CH").unwrap_or(5.0) as usize;
        let pwm = cx.fc.rc_channel(ch).ok_or("no_rc")?;
        let slot = mode_slot(pwm);
        let (guided, _) = cx.adapter.mode(Mode::Guided).ok_or("unsupported")?;
        cx.params.set(&format!("FLTMODE{slot}"), guided as f32);
        self.writing = Some(slot);
        self.core.running();
        Ok(())
    }
}
