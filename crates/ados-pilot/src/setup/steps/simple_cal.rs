//! `level`, `gyro` and `airspeed`: one PREFLIGHT_CALIBRATION each.
//!
//! ArduPilot runs these synchronously and answers with the final ACK; PX4
//! acknowledges at once and reports through `[cal]` STATUSTEXT.

use ados_protocol::mavlink::ardupilotmega::{MavCmd, MavResult};
use serde_json::{json, Map, Value};

use crate::setup::adapters::px4::{parse_cal, CalEvent};
use crate::setup::fcstate::{Class, Fw};
use crate::setup::schema::{obj, Action};
use crate::setup::step::{ActErr, Cx, Step, StepCore};

const CMD_241: u32 = 241;
const TIMEOUT_MS: u64 = 120_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Level,
    Gyro,
    Airspeed,
}

pub struct SimpleCal {
    core: StepCore,
    kind: Kind,
    started_ms: Option<u64>,
    text_cursor: u64,
}

impl SimpleCal {
    pub fn new(kind: Kind) -> Self {
        SimpleCal { core: StepCore::default(), kind, started_ms: None, text_cursor: 0 }
    }

    fn params(&self) -> [f32; 7] {
        match self.kind {
            Kind::Level => [0.0, 0.0, 0.0, 0.0, 2.0, 0.0, 0.0],
            Kind::Gyro => [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            // ArduPilot: p6 = 2 is airspeed; PX4 reads p6 = 1.
            Kind::Airspeed => [0.0; 7],
        }
    }
}

impl Step for SimpleCal {
    fn id(&self) -> &'static str {
        match self.kind {
            Kind::Level => "level",
            Kind::Gyro => "gyro",
            Kind::Airspeed => "airspeed",
        }
    }

    fn core(&self) -> &StepCore {
        &self.core
    }

    fn core_mut(&mut self) -> &mut StepCore {
        &mut self.core
    }

    fn optional(&self) -> bool {
        true
    }

    fn skip_reason(&self, cx: &Cx) -> Option<&'static str> {
        match self.kind {
            Kind::Airspeed => {
                if cx.adapter.class == Class::Multirotor {
                    return Some("not_applicable");
                }
                let present = if cx.adapter.fw == Fw::Px4 {
                    cx.params.get("SENS_EN_SDP3X").unwrap_or(0.0) != 0.0
                        || cx.params.get("SENS_EN_MS4525DO").unwrap_or(0.0) != 0.0
                        || cx.params.get("SENS_EN_MS5525DS").unwrap_or(0.0) != 0.0
                } else {
                    cx.params.get("ARSPD_TYPE").unwrap_or(0.0) != 0.0
                };
                (!present).then_some("no_airspeed_sensor")
            }
            _ => None,
        }
    }

    fn begin(&mut self, cx: &mut Cx) {
        self.core = StepCore::default();
        self.started_ms = None;
        match self.kind {
            Kind::Level => {
                if cx.adapter.class == Class::Fixedwing {
                    self.core.body_variant = Some("plane");
                    self.core.image = Some("level_plane".into());
                    let trim = cx.params.get("PTCH_TRIM_DEG").unwrap_or(0.0);
                    self.core.args.insert("pitch_trim_deg".into(), json!(trim));
                } else {
                    self.core.image = Some("accel_level".into());
                }
            }
            Kind::Airspeed => self.core.image = Some("airspeed_cover".into()),
            Kind::Gyro => {}
        }
        self.core.wait_user(vec![Action::primary("start"), Action::secondary("skip")]);
    }

    fn tick(&mut self, cx: &mut Cx) {
        let Some(started) = self.started_ms else { return };
        if self.core.state.is_terminal() {
            return;
        }
        if let Some(r) = cx.fc.ack_since(CMD_241, started) {
            match r {
                MavResult::MAV_RESULT_ACCEPTED if cx.adapter.fw != Fw::Px4 => {
                    self.core.done(obj([("ack", json!("accepted"))]));
                    return;
                }
                MavResult::MAV_RESULT_ACCEPTED | MavResult::MAV_RESULT_IN_PROGRESS => {}
                _ => {
                    self.core.fail("setup.blocker.calibration_refused");
                    return;
                }
            }
        }
        let (texts, cursor) = cx.fc.texts_after(self.text_cursor);
        self.text_cursor = cursor;
        for t in texts {
            match parse_cal(&t) {
                Some(CalEvent::Progress(p)) => self.core.progress = Some(p as f32 / 100.0),
                Some(CalEvent::Done) => {
                    self.core.done(obj([("text", json!(t))]));
                    return;
                }
                Some(CalEvent::Failed(_)) => {
                    self.core.fail("setup.blocker.calibration_failed");
                    return;
                }
                _ => {}
            }
        }
        if cx.now.saturating_sub(started) > TIMEOUT_MS {
            self.core.fail("setup.blocker.timeout");
        }
    }

    fn action(&mut self, id: &str, _args: &Map<String, Value>, cx: &mut Cx) -> Result<(), ActErr> {
        if id != "start" {
            return Err("unknown_action");
        }
        if cx.fc.armed {
            return Err("armed");
        }
        let mut p = self.params();
        if self.kind == Kind::Airspeed {
            p[5] = if cx.adapter.fw == Fw::Px4 { 1.0 } else { 2.0 };
        }
        cx.out.cmd(MavCmd::MAV_CMD_PREFLIGHT_CALIBRATION, p);
        self.started_ms = Some(cx.now);
        self.text_cursor = cx.fc.text_seq;
        self.core.running();
        Ok(())
    }
}
