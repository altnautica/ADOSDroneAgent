//! The plane autotune phases and the closing gain report.

use super::*;

impl Autotune {
    pub(super) fn plane_position(&mut self, cx: &mut Cx) {
        self.to.tick(cx);
        if let Some(w) = self.to.failed {
            self.phase = Phase::Idle;
            self.core.fail(&format!("setup.blocker.{w}"));
        } else if self.to.ready {
            cx.params.set("AUTOTUNE_LEVEL", PLANE_LEVEL);
            self.text_cursor = cx.fc.text_seq;
            let roll_lim = cx
                .params
                .get("ROLL_LIMIT_DEG")
                .or_else(|| cx.params.get("LIM_ROLL_CD").map(|v| v / 100.0))
                .unwrap_or(45.0);
            let pitch_lim = cx
                .params
                .get("PTCH_LIM_MAX_DEG")
                .or_else(|| cx.params.get("LIM_PITCH_MAX").map(|v| v / 100.0))
                .unwrap_or(20.0);
            self.doublets = Some(Doublets::new(
                cx.now,
                roll_lim,
                pitch_lim,
                cx.adapter.tuning_box_m(true),
            ));
            flight::set_mode(cx, Mode::Autotune);
            self.phase = Phase::PlaneTuning { since: cx.now };
        }
    }

    pub(super) fn plane_tuning(&mut self, cx: &mut Cx, since: u64, axes: [f32; 4]) {
        let thr = cx
            .data
            .cruise_throttle
            .or_else(|| cx.params.get("TRIM_THROTTLE"))
            .unwrap_or(45.0)
            / 100.0;
        let off = match (cx.fc.local, cx.data.box_center) {
            (Some((l, _)), Some(c)) => [l[0] - c[0], l[1] - c[1]],
            _ => [0.0, 0.0],
        };
        let heading = cx.fc.attitude.map(|(a, _)| a[2]).unwrap_or(0.0);
        let operator = axes.iter().any(|a| a.abs() > BLEND);
        let (x, y, r) = if operator {
            let (x, y, _, r) = blended(axes);
            (x, y, r)
        } else if let Some(d) = self.doublets.as_mut() {
            let alt_err = height(cx).map_or(0.0, |h| PLANE_LOITER_ALT_M - h);
            let dm = d.demand(cx.now, off, heading, alt_err);
            // MANUAL_CONTROL x > 0 is nose down.
            (-dm.pitch, dm.roll, 0.0)
        } else {
            (0.0, 0.0, 0.0)
        };
        self.manual(cx, x, y, thr, r);
        let el = cx.now.saturating_sub(since);
        if el > 3000 && !flight::in_mode(cx, Mode::Autotune) && el % 2000 < MANUAL_MS {
            flight::set_mode(cx, Mode::Autotune);
        }
        for e in self.tick_texts(cx) {
            if let TuneEvent::AxisDone(a) = e {
                if !self.plane_done.contains(&a) {
                    self.plane_done.push(a);
                }
            }
        }
        self.core.progress = Some(0.5 + 0.5 * (el as f32 / PLANE_MAX_MS as f32).min(1.0));
        let both = self.plane_done.contains(&Axis::Roll) && self.plane_done.contains(&Axis::Pitch);
        if both || el >= PLANE_MAX_MS {
            if !both {
                self.notes.push(
                    json!({"plane": "time_limit", "axes_done": format!("{:?}", self.plane_done)}),
                );
            }
            plane_loiter(cx, PLANE_LOITER_ALT_M);
            self.last_poll = cx.now;
            self.phase = Phase::PlaneExit;
        }
    }

    pub(super) fn plane_exit(&mut self, cx: &mut Cx) {
        if flight::in_mode(cx, Mode::Guided) {
            self.read_gains(cx);
        } else if cx.now.saturating_sub(self.last_poll) > 3000 {
            self.last_poll = cx.now;
            plane_loiter(cx, PLANE_LOITER_ALT_M);
        }
    }

    /// Read the gains back and finish with the before/after report.
    pub(super) fn report_gains(&mut self, cx: &mut Cx) {
        if !cx.params.idle() {
            return;
        }
        let after: Map<String, Value> = cx
            .adapter
            .report_gains()
            .into_iter()
            .filter_map(|n| cx.params.get(&n).map(|v| (n, json!(v))))
            .collect();
        let changed: Vec<String> = after
            .iter()
            .filter(|(k, v)| self.before.get(*k).is_some_and(|b| b != *v))
            .map(|(k, _)| k.clone())
            .collect();
        let copter_axes: Vec<String> = self
            .copter_done
            .iter()
            .map(|a| format!("{a:?}").to_lowercase())
            .collect();
        let plane_axes: Vec<String> = self
            .plane_done
            .iter()
            .map(|a| format!("{a:?}").to_lowercase())
            .collect();
        self.phase = Phase::Idle;
        self.core.phase_key = None;
        self.core.done(obj([
            (
                "gains_before",
                Value::Object(std::mem::take(&mut self.before)),
            ),
            ("gains_after", Value::Object(after)),
            ("changed", json!(changed)),
            ("copter_axes_saved", json!(copter_axes)),
            ("plane_axes_done", json!(plane_axes)),
            ("yaw_pending", json!(cx.data.autotune_yaw_pending)),
            ("notes", Value::Array(std::mem::take(&mut self.notes))),
        ]));
    }
}
