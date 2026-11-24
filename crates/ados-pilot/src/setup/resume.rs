//! A wizard that was running when the agent stopped: marked `interrupted` on
//! load, continued by `resume` at its first unfinished step.

use serde_json::{json, Value};

use super::schema::{StepState, WizardId, WizardState};
use super::{reply_err, Engine};

impl Engine {
    /// A wizard the store says is running has no live run once the agent has
    /// restarted: it is interrupted. Its step records stay as they were.
    pub(super) fn mark_interrupted(&mut self) {
        let live = self.run.as_ref().map(|r| r.wizard);
        for (w, wr) in [
            (WizardId::Ground, &mut self.state.ground),
            (WizardId::Air, &mut self.state.air),
        ] {
            if wr.state == WizardState::Running && live != Some(w) {
                wr.state = WizardState::Interrupted;
                self.wizard_dirty = true;
            }
        }
    }

    /// Continue an interrupted wizard at the first step its records do not
    /// show done or skipped, keeping every record. Refused as `start` is.
    pub fn resume(&mut self, w: WizardId, now: u64) -> Value {
        let wr = match w {
            WizardId::Ground => &self.state.ground,
            WizardId::Air => &self.state.air,
        };
        if wr.state != WizardState::Interrupted {
            return reply_err("not_interrupted");
        }
        let b = self.wizard_blockers(w, now);
        if let Some(first) = b.first() {
            return reply_err(first.trim_start_matches("setup.blocker."));
        }
        let Some(mut run) = self.new_run(w) else {
            return reply_err("detecting");
        };
        // Without an identity detection has not settled: run it again first.
        if self.uid.is_some() {
            let finished = |id: &str| {
                wr.steps
                    .get(id)
                    .and_then(|r| r.get("state"))
                    .and_then(|s| serde_json::from_value::<StepState>(s.clone()).ok())
                    .is_some_and(|s| matches!(s, StepState::Done | StepState::Skipped))
            };
            run.idx = run
                .steps
                .iter()
                .position(|s| !finished(s.id()))
                .unwrap_or(run.steps.len());
        }
        self.run = Some(run);
        self.last_abort = None;
        let wr = match w {
            WizardId::Ground => &mut self.state.ground,
            WizardId::Air => &mut self.state.air,
        };
        wr.state = WizardState::Running;
        self.begin_current(now);
        self.persist(now);
        self.wizard_dirty = true;
        json!({"ok": true, "wizard": self.wizard_view(w, now)})
    }
}
