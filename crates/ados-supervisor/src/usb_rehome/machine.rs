//! Pure trigger debounce + bounded retry/cooldown state machine for the
//! USB-rehome self-heal. No OS calls — both are unit-tested on every host.

use std::time::{Duration, Instant};

use ados_protocol::logd::{Fields, Value as MpVal};

/// The event kind recorded for a rehome attempt + outcome.
pub const USB_REHOME_KIND: &str = "radio.usb_rehome";

/// How long the dual-signal condition (slow USB port AND unverified RF) must
/// hold continuously before a rehome is authorized. Longer than the RF-unverified
/// hold because a rehome stops the radio — a high bar keeps it off a transient.
pub const REHOME_CONFIRM_HOLD: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrigState {
    Clear,
    Pending(Instant),
    Armed,
}

/// Debounced arming signal for the rehome. Fed `usb_degraded && rf_unverified`
/// each tick; reports armed (a level, not a one-shot edge) once the condition
/// has held continuously for the confirm window, so the retry machine can pace
/// multiple attempts while the fault persists. Resets the moment the condition
/// releases, so a recovered adapter re-arms cleanly on a fresh fault.
#[derive(Debug, Clone, Copy)]
pub struct RehomeTrigger {
    state: TrigState,
    hold: Duration,
}

impl RehomeTrigger {
    pub fn new() -> Self {
        Self {
            state: TrigState::Clear,
            hold: REHOME_CONFIRM_HOLD,
        }
    }

    pub fn with_hold(hold: Duration) -> Self {
        Self {
            state: TrigState::Clear,
            hold,
        }
    }

    /// Feed the instantaneous dual-signal condition. Returns whether the trigger
    /// is currently armed (the condition has held the full window). Pure aside
    /// from `now`.
    pub fn observe(&mut self, cond: bool, now: Instant) -> bool {
        match self.state {
            TrigState::Clear => {
                if cond {
                    self.state = TrigState::Pending(now);
                }
                false
            }
            TrigState::Pending(since) => {
                if !cond {
                    self.state = TrigState::Clear;
                    false
                } else if now.saturating_duration_since(since) >= self.hold {
                    self.state = TrigState::Armed;
                    true
                } else {
                    false
                }
            }
            TrigState::Armed => {
                if !cond {
                    self.state = TrigState::Clear;
                    false
                } else {
                    true
                }
            }
        }
    }
}

impl Default for RehomeTrigger {
    fn default() -> Self {
        Self::new()
    }
}

/// What the retry machine decided this tick. Pure so the budget + cooldown +
/// anti-flap contract is tested without OS calls or a real clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RehomeStep {
    /// Nothing to do (healthy, or armed-but-mid-cooldown handled separately).
    Idle,
    /// Fire a rehome attempt now (1-based index).
    Attempt { index: u32 },
    /// Armed but inside the post-attempt cooldown: wait.
    Cooldown { remaining_s: u64 },
    /// The attempt budget is spent and the fault persists: parked.
    Exhausted,
    /// The adapter verified healthy for the full reset window: episode reset.
    Recovered,
}

/// Bounded retry + cooldown + anti-flap state machine for one rehome episode.
#[derive(Debug, Clone, Default)]
pub struct RehomeMachine {
    attempts: u32,
    last_attempt: Option<Instant>,
    healthy_since: Option<Instant>,
    exhausted: bool,
}

impl RehomeMachine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    pub fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    /// Undo the most recent attempt (the guard refused it, so it must not count
    /// against the budget). Clears the last-attempt timestamp so a re-evaluation
    /// is not blocked by a phantom cooldown.
    pub fn refund_attempt(&mut self) {
        self.attempts = self.attempts.saturating_sub(1);
        self.last_attempt = None;
    }

    /// One step. `armed` is the trigger level; `verified_healthy` is the
    /// post-rehome good state (high-speed USB AND reception confirmed) read from
    /// the fresh sidecar this tick. `cooldown_for(n)` is the wait owed after the
    /// n-th attempt; `healthy_reset` is the sustained-healthy window that resets
    /// the episode budget (anti-flap). Pure.
    pub fn step(
        &mut self,
        armed: bool,
        verified_healthy: bool,
        max_attempts: u32,
        cooldown_for: impl Fn(u32) -> Duration,
        healthy_reset: Duration,
        now: Instant,
    ) -> RehomeStep {
        if verified_healthy {
            // Track sustained health; only reset the episode after the full
            // window so a flapping adapter cannot earn a fresh budget every few
            // seconds.
            let healthy_since = *self.healthy_since.get_or_insert(now);
            if now.saturating_duration_since(healthy_since) >= healthy_reset {
                let was_mid_episode = self.attempts > 0 || self.exhausted;
                self.attempts = 0;
                self.last_attempt = None;
                self.healthy_since = None;
                self.exhausted = false;
                return if was_mid_episode {
                    RehomeStep::Recovered
                } else {
                    RehomeStep::Idle
                };
            }
            return RehomeStep::Idle;
        }

        // Still faulty: reset the healthy timer.
        self.healthy_since = None;
        if !armed {
            return RehomeStep::Idle;
        }
        if self.exhausted {
            return RehomeStep::Exhausted;
        }
        if self.attempts >= max_attempts {
            self.exhausted = true;
            return RehomeStep::Exhausted;
        }
        // Respect the cooldown owed after the most recent attempt.
        if self.attempts > 0 {
            if let Some(last) = self.last_attempt {
                let cd = cooldown_for(self.attempts);
                let elapsed = now.saturating_duration_since(last);
                if elapsed < cd {
                    return RehomeStep::Cooldown {
                        remaining_s: (cd - elapsed).as_secs(),
                    };
                }
            }
        }
        self.attempts += 1;
        self.last_attempt = Some(now);
        RehomeStep::Attempt {
            index: self.attempts,
        }
    }
}

/// Build the `radio.usb_rehome` detail map. Bland fields. Pure.
#[allow(clippy::too_many_arguments)]
pub fn usb_rehome_detail(
    state: &str,
    iface: &str,
    bind_id: &str,
    attempt: u32,
    max_attempts: u32,
    before_speed_mbps: Option<u32>,
    after_speed_mbps: Option<u32>,
    reason: Option<&str>,
) -> Fields {
    let mut d = Fields::new();
    d.insert("state".to_string(), MpVal::from(state));
    d.insert("iface".to_string(), MpVal::from(iface));
    d.insert("bind_id".to_string(), MpVal::from(bind_id));
    d.insert("attempt".to_string(), MpVal::from(attempt as u64));
    d.insert("max_attempts".to_string(), MpVal::from(max_attempts as u64));
    if let Some(s) = before_speed_mbps {
        d.insert("before_usb_speed_mbps".to_string(), MpVal::from(s as u64));
    }
    if let Some(s) = after_speed_mbps {
        d.insert("after_usb_speed_mbps".to_string(), MpVal::from(s as u64));
    }
    if let Some(r) = reason {
        d.insert("reason".to_string(), MpVal::from(r));
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_arms_after_hold_and_resets_on_release() {
        let hold = Duration::from_secs(30);
        let mut t = RehomeTrigger::with_hold(hold);
        let t0 = Instant::now();
        // Holding but inside the window: not armed.
        assert!(!t.observe(true, t0));
        assert!(!t.observe(true, t0 + Duration::from_secs(15)));
        // Past the window: armed (a level), stays armed while the fault holds.
        assert!(t.observe(true, t0 + Duration::from_secs(30)));
        assert!(t.observe(true, t0 + Duration::from_secs(40)));
        // Condition releases → disarms; a fresh onset must re-run the window.
        assert!(!t.observe(false, t0 + Duration::from_secs(45)));
        assert!(!t.observe(true, t0 + Duration::from_secs(46)));
        assert!(t.observe(true, t0 + Duration::from_secs(76)));
    }

    #[test]
    fn trigger_does_not_arm_on_a_transient() {
        let hold = Duration::from_secs(30);
        let mut t = RehomeTrigger::with_hold(hold);
        let t0 = Instant::now();
        assert!(!t.observe(true, t0));
        // Released before the window: never armed.
        assert!(!t.observe(false, t0 + Duration::from_secs(10)));
        assert!(!t.observe(true, t0 + Duration::from_secs(11)));
    }

    fn cd(_n: u32) -> Duration {
        Duration::from_secs(10)
    }

    #[test]
    fn fires_one_attempt_then_cools_down() {
        let mut m = RehomeMachine::new();
        let t0 = Instant::now();
        // Armed + faulty + no prior attempt → fire attempt 1 immediately.
        assert_eq!(
            m.step(true, false, 3, cd, Duration::from_secs(120), t0),
            RehomeStep::Attempt { index: 1 }
        );
        // Inside the cooldown: wait.
        match m.step(
            true,
            false,
            3,
            cd,
            Duration::from_secs(120),
            t0 + Duration::from_secs(3),
        ) {
            RehomeStep::Cooldown { .. } => {}
            other => panic!("expected cooldown, got {other:?}"),
        }
        // After the cooldown: attempt 2.
        assert_eq!(
            m.step(
                true,
                false,
                3,
                cd,
                Duration::from_secs(120),
                t0 + Duration::from_secs(11)
            ),
            RehomeStep::Attempt { index: 2 }
        );
    }

    #[test]
    fn budget_caps_to_exhausted_once() {
        let mut m = RehomeMachine::new();
        let t0 = Instant::now();
        m.step(true, false, 2, cd, Duration::from_secs(120), t0);
        m.step(
            true,
            false,
            2,
            cd,
            Duration::from_secs(120),
            t0 + Duration::from_secs(11),
        );
        // Budget (2) spent → exhausted, and it stays exhausted (no re-loop).
        assert_eq!(
            m.step(
                true,
                false,
                2,
                cd,
                Duration::from_secs(120),
                t0 + Duration::from_secs(22)
            ),
            RehomeStep::Exhausted
        );
        assert!(m.is_exhausted());
        assert_eq!(
            m.step(
                true,
                false,
                2,
                cd,
                Duration::from_secs(120),
                t0 + Duration::from_secs(40)
            ),
            RehomeStep::Exhausted
        );
    }

    #[test]
    fn not_armed_is_idle() {
        let mut m = RehomeMachine::new();
        let t0 = Instant::now();
        assert_eq!(
            m.step(false, false, 3, cd, Duration::from_secs(120), t0),
            RehomeStep::Idle
        );
    }

    #[test]
    fn sustained_health_recovers_and_resets_the_budget() {
        let mut m = RehomeMachine::new();
        let t0 = Instant::now();
        // Spend an attempt.
        m.step(true, false, 3, cd, Duration::from_secs(120), t0);
        // Healthy but not yet for the reset window → Idle, budget not reset.
        assert_eq!(
            m.step(
                false,
                true,
                3,
                cd,
                Duration::from_secs(120),
                t0 + Duration::from_secs(60)
            ),
            RehomeStep::Idle
        );
        // Healthy past the reset window → Recovered + episode reset.
        assert_eq!(
            m.step(
                false,
                true,
                3,
                cd,
                Duration::from_secs(120),
                t0 + Duration::from_secs(181)
            ),
            RehomeStep::Recovered
        );
        assert_eq!(m.attempts(), 0);
        // A fresh fault can attempt again from index 1.
        assert_eq!(
            m.step(
                true,
                false,
                3,
                cd,
                Duration::from_secs(120),
                t0 + Duration::from_secs(200)
            ),
            RehomeStep::Attempt { index: 1 }
        );
    }

    #[test]
    fn a_brief_healthy_blip_does_not_reset_mid_episode() {
        let mut m = RehomeMachine::new();
        let t0 = Instant::now();
        m.step(true, false, 3, cd, Duration::from_secs(120), t0);
        // Brief health (under the reset window), then faulty again: the healthy
        // timer resets so the next sustained-health window starts fresh, and the
        // attempt count is preserved (not reset by the blip).
        m.step(
            false,
            true,
            3,
            cd,
            Duration::from_secs(120),
            t0 + Duration::from_secs(20),
        );
        assert_eq!(m.attempts(), 1);
        // Faulty again, cooldown elapsed → attempt 2 (budget was not reset).
        assert_eq!(
            m.step(
                true,
                false,
                3,
                cd,
                Duration::from_secs(120),
                t0 + Duration::from_secs(40)
            ),
            RehomeStep::Attempt { index: 2 }
        );
    }

    #[test]
    fn detail_is_bland_and_omits_absent_fields() {
        let d = usb_rehome_detail("rehoming", "wlan1", "1-1", 1, 3, Some(12), None, None);
        assert_eq!(d.get("state").and_then(|v| v.as_str()), Some("rehoming"));
        assert_eq!(d.get("bind_id").and_then(|v| v.as_str()), Some("1-1"));
        assert_eq!(
            d.get("before_usb_speed_mbps").and_then(|v| v.as_u64()),
            Some(12)
        );
        assert!(!d.contains_key("after_usb_speed_mbps"));
        assert!(!d.contains_key("reason"));
        let g = usb_rehome_detail(
            "guard_blocked",
            "wlan1",
            "1-1",
            0,
            3,
            None,
            None,
            Some("shares_device"),
        );
        assert_eq!(
            g.get("reason").and_then(|v| v.as_str()),
            Some("shares_device")
        );
    }
}
