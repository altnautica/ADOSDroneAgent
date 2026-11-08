//! Mode, gain and stick helpers the autotune phases share.

use super::*;

impl Autotune {
    pub(super) fn tune_mode(&self, cx: &Cx) -> Mode {
        match (cx.adapter.fw, cx.adapter.class, self.stage) {
            (Fw::Px4, _, _) => Mode::Posctl,
            (_, Class::Vtol, Stage::Copter) => Mode::Qautotune,
            _ => Mode::Autotune,
        }
    }

    /// The position-hold mode an ArduPilot copter-style tune is entered from.
    /// ArduCopter refuses AUTOTUNE from GUIDED ("init failed": only modes
    /// with `allows_autotune`, i.e. STABILIZE, ALT_HOLD, LOITER, POSHOLD),
    /// and from LOITER it keeps position between twitches; QAUTOTUNE holds
    /// position when entered from QLOITER.
    pub(super) fn hold_mode(&self, cx: &Cx) -> Option<Mode> {
        match (cx.adapter.fw, cx.adapter.class) {
            (Fw::ArduPilot, Class::Multirotor) => Some(Mode::Loiter),
            (Fw::ArduPilot, Class::Vtol) => Some(Mode::Qloiter),
            _ => None,
        }
    }

    /// The FC would not take the tune mode: land and say so.
    pub(super) fn refused(&mut self, cx: &mut Cx) {
        self.hop.land(cx);
        self.phase = Phase::Idle;
        self.core.fail("setup.blocker.tuning_refused");
    }

    pub(super) fn axes_param(&self, cx: &Cx) -> String {
        format!("{}_AXES", cx.adapter.autotune_prefix())
    }

    pub(super) fn manual(&mut self, cx: &mut Cx, x: f32, y: f32, z: f32, r: f32) {
        if cx.now.saturating_sub(self.last_manual) >= MANUAL_MS {
            self.last_manual = cx.now;
            cx.out.manual(x, y, z, r);
        }
    }

    pub(super) fn snapshot_gains(&mut self, cx: &Cx) {
        self.before = cx
            .adapter
            .report_gains()
            .into_iter()
            .filter_map(|n| cx.params.get(&n).map(|v| (n, json!(v))))
            .collect();
    }

    pub(super) fn start_copter_round(&mut self, cx: &mut Cx) {
        self.stage = Stage::Copter;
        self.hop.start(cx, TUNE_ALT_M);
        self.phase = Phase::TakeOff;
        self.core.phase_key = Some(if self.yaw_round { "yaw" } else { "roll_pitch" }.into());
    }

    pub(super) fn start_plane(&mut self, cx: &mut Cx) {
        self.stage = Stage::Plane;
        self.to.start(cx);
        self.phase = Phase::PlanePosition;
        self.core.phase_key = Some("plane".into());
    }

    pub(super) fn after_copter(&mut self, cx: &mut Cx) {
        if !self.yaw_round && cx.adapter.fw == Fw::ArduPilot {
            if cx.fc.battery_pct.is_some_and(|p| p >= YAW_MIN_BATTERY) {
                self.yaw_round = true;
                self.start_copter_round(cx);
            } else {
                cx.data.autotune_yaw_pending = true;
                self.phase = Phase::YawWait;
                self.core.phase_key = Some("battery_swap".into());
                self.core.wait_user(vec![
                    Action::primary("resume_yaw"),
                    Action::secondary("skip_yaw"),
                ]);
            }
            return;
        }
        cx.data.autotune_yaw_pending = false;
        if cx.adapter.class == Class::Vtol {
            self.start_plane(cx);
        } else {
            self.read_gains(cx);
        }
    }

    pub(super) fn read_gains(&mut self, cx: &mut Cx) {
        for n in cx.adapter.report_gains() {
            if cx.params.has(&n) {
                cx.params.read(&n);
            }
        }
        self.phase = Phase::ReadGains;
    }

    pub(super) fn tick_texts(&mut self, cx: &mut Cx) -> Vec<TuneEvent> {
        let (texts, cursor) = cx.fc.texts_after(self.text_cursor);
        self.text_cursor = cursor;
        let mut ev = Vec::new();
        for t in texts {
            if let Some(e) = parse_tune(&t) {
                self.core.set_live("last_text", t.clone());
                ev.push(e);
            }
        }
        ev
    }
}
