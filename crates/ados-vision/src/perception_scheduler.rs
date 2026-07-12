//! Perception scheduler: arbitrates several engine-run models over the ONE
//! shared accelerator so multiple perception tasks coexist on one NPU.
//!
//! The vision engine serializes all inference behind a single accelerator lease
//! (the NPU runs one model at a time — a hardware reality). With only that lease
//! and one auto-run detector, a second task (a depth pass, a segmentation pass)
//! cannot run alongside the detector. This scheduler is the arbitration layer in
//! front of the lease: each engine-run model gets a target rate and a priority,
//! and the scheduler decides which model is DUE to run at a given moment. The
//! engine's capture loop consults it each frame and runs the due model(s) on the
//! lease, so e.g. `person@15Hz` and `depth@5Hz` interleave on one accelerator
//! instead of one detector starving the rest.
//!
//! Pure and I/O-free: it holds only per-model timing state and is driven by a
//! monotonic millisecond clock the caller supplies (`time.monotonic()`-style),
//! so its behaviour is deterministic and unit-testable without a real NPU. The
//! live engine-loop integration (running the due models on the lease) is layered
//! on top and validated on hardware.

use std::collections::HashMap;

/// A model the scheduler paces: its rate budget + priority + last-run stamp.
#[derive(Debug, Clone)]
struct Scheduled {
    /// Minimum interval between runs, derived from the target rate. `0` means
    /// "every tick" (run as often as the loop offers).
    min_interval_ms: u64,
    /// Higher runs first when several models are due at once.
    priority: u8,
    /// Monotonic ms of the last time this model was marked run; `None` = never.
    last_run_ms: Option<u64>,
}

/// Arbitrates engine-run models over the shared accelerator by rate + priority.
#[derive(Debug, Default)]
pub struct PerceptionScheduler {
    models: HashMap<String, Scheduled>,
}

/// Convert a target rate in Hz to a minimum inter-run interval in ms. A
/// non-positive or non-finite rate means "unpaced" (run every tick).
fn interval_ms_for_rate(rate_hz: f32) -> u64 {
    if rate_hz.is_finite() && rate_hz > 0.0 {
        (1000.0 / rate_hz).round() as u64
    } else {
        0
    }
}

impl PerceptionScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of models currently scheduled.
    pub fn len(&self) -> usize {
        self.models.len()
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    /// Add or update a model's rate + priority. Re-inserting keeps the model's
    /// last-run stamp so a rate change does not reset its cadence.
    pub fn upsert(&mut self, model_id: impl Into<String>, rate_hz: f32, priority: u8) {
        let id = model_id.into();
        let min_interval_ms = interval_ms_for_rate(rate_hz);
        match self.models.get_mut(&id) {
            Some(m) => {
                m.min_interval_ms = min_interval_ms;
                m.priority = priority;
            }
            None => {
                self.models.insert(
                    id,
                    Scheduled {
                        min_interval_ms,
                        priority,
                        last_run_ms: None,
                    },
                );
            }
        }
    }

    /// Remove a model from the schedule.
    pub fn remove(&mut self, model_id: &str) {
        self.models.remove(model_id);
    }

    /// Whether `model_id` is due to run at `now_ms` (never run, or its interval
    /// has elapsed). Unknown models are not due.
    fn is_due(&self, model: &Scheduled, now_ms: u64) -> bool {
        match model.last_run_ms {
            None => true,
            Some(last) => now_ms.saturating_sub(last) >= model.min_interval_ms,
        }
    }

    /// The models due to run at `now_ms`, highest priority first (ties broken by
    /// model id for determinism). Marks every returned model as run at `now_ms`.
    /// Use this when the loop can run several models before the next tick.
    pub fn take_due(&mut self, now_ms: u64) -> Vec<String> {
        let mut due: Vec<(u8, String)> = self
            .models
            .iter()
            .filter(|(_, m)| self.is_due(m, now_ms))
            .map(|(id, m)| (m.priority, id.clone()))
            .collect();
        // Highest priority first; then model id ascending for a stable order.
        due.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let ids: Vec<String> = due.into_iter().map(|(_, id)| id).collect();
        for id in &ids {
            if let Some(m) = self.models.get_mut(id) {
                m.last_run_ms = Some(now_ms);
            }
        }
        ids
    }

    /// The single highest-priority due model at `now_ms`, marking only it as run.
    /// Use this when the loop runs exactly one model per tick on the lease.
    pub fn next_due(&mut self, now_ms: u64) -> Option<String> {
        let chosen = self
            .models
            .iter()
            .filter(|(_, m)| self.is_due(m, now_ms))
            .max_by(|a, b| a.1.priority.cmp(&b.1.priority).then_with(|| b.0.cmp(a.0)))
            .map(|(id, _)| id.clone());
        if let Some(id) = &chosen {
            if let Some(m) = self.models.get_mut(id) {
                m.last_run_ms = Some(now_ms);
            }
        }
        chosen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_to_interval() {
        assert_eq!(interval_ms_for_rate(15.0), 67); // ~66.7 -> 67
        assert_eq!(interval_ms_for_rate(5.0), 200);
        assert_eq!(interval_ms_for_rate(0.0), 0); // unpaced
        assert_eq!(interval_ms_for_rate(-1.0), 0);
    }

    #[test]
    fn a_new_model_is_immediately_due() {
        let mut s = PerceptionScheduler::new();
        s.upsert("person", 15.0, 10);
        assert_eq!(s.take_due(1000), vec!["person"]);
        // Just ran: not due again until the interval elapses.
        assert!(s.take_due(1000).is_empty());
        assert!(s.take_due(1050).is_empty()); // < 67 ms
        assert_eq!(s.take_due(1067), vec!["person"]);
    }

    #[test]
    fn faster_model_runs_more_often_than_slower() {
        let mut s = PerceptionScheduler::new();
        s.upsert("person", 10.0, 10); // 100 ms
        s.upsert("depth", 2.0, 5); //   500 ms
                                   // t=0: both due. Person (higher priority) first.
        assert_eq!(s.take_due(0), vec!["person", "depth"]);
        // t=100: only person due.
        assert_eq!(s.take_due(100), vec!["person"]);
        assert_eq!(s.take_due(200), vec!["person"]);
        assert_eq!(s.take_due(300), vec!["person"]);
        assert_eq!(s.take_due(400), vec!["person"]);
        // t=500: person AND depth due again.
        assert_eq!(s.take_due(500), vec!["person", "depth"]);
    }

    #[test]
    fn priority_orders_simultaneously_due_models() {
        let mut s = PerceptionScheduler::new();
        s.upsert("low", 30.0, 1);
        s.upsert("high", 30.0, 9);
        s.upsert("mid", 30.0, 5);
        assert_eq!(s.take_due(0), vec!["high", "mid", "low"]);
    }

    #[test]
    fn next_due_runs_one_highest_priority_model() {
        let mut s = PerceptionScheduler::new();
        s.upsert("a", 30.0, 3);
        s.upsert("b", 30.0, 7);
        // Only the top-priority due model runs; the other stays due.
        assert_eq!(s.next_due(0).as_deref(), Some("b"));
        assert_eq!(s.next_due(0).as_deref(), Some("a"));
        assert_eq!(s.next_due(0), None); // both ran at t=0
    }

    #[test]
    fn upsert_updates_rate_without_resetting_cadence() {
        let mut s = PerceptionScheduler::new();
        s.upsert("m", 10.0, 5); // 100 ms
        assert_eq!(s.take_due(0), vec!["m"]);
        // Speed it up; the last-run stamp is preserved, so at t=50 (now under
        // the new 20 ms interval since t=0) it is due again.
        s.upsert("m", 20.0, 5); // 50 ms
        assert_eq!(s.take_due(50), vec!["m"]);
    }

    #[test]
    fn remove_takes_a_model_off_the_schedule() {
        let mut s = PerceptionScheduler::new();
        s.upsert("m", 10.0, 5);
        assert_eq!(s.len(), 1);
        s.remove("m");
        assert!(s.is_empty());
        assert!(s.take_due(0).is_empty());
    }
}
