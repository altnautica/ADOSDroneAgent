//! Pure trigger debounce + retry/cooldown state machine for the USB-rehome
//! self-heal. No OS calls — both are unit-tested on every host.
//!
//! ## Why there is no attempt budget any more
//!
//! This machine used to spend a budget of three attempts and then latch an
//! `exhausted` state, and the latch was unescapable by construction. The only
//! exit was a sustained-healthy window; the adapter can only become healthy
//! after a successful rehome; and `exhausted` was precisely the state in which
//! no further rehome is attempted. So a genuinely wedged USB radio — the exact
//! fault this self-heal exists to repair — was abandoned after ninety seconds
//! and stayed abandoned until someone rebooted the vehicle or cleared it over
//! SSH. On a drone in a field those are the same thing as a dead vehicle.
//!
//! The budget also carried an escalating `[10, 30, 60]` s cooldown. Growth is
//! the reflex answer for a client of a shared remote service; it is the wrong
//! answer for a board recovering its own peripheral, where there is no herd to
//! spread and the growth only delays recovery.
//!
//! So: retry forever, at a fixed interval, and report the attempt count so an
//! operator can see the vehicle is still trying and how long it has been
//! trying. What the machine keeps is the part that was load-bearing — the
//! anti-flap `healthy_reset` window, which stops a flapping adapter from
//! churning the episode, and the cooldown itself, which stops a rehome (a real
//! USB unbind/rebind) from thrashing the bus.

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

/// What the retry machine decided this tick. Pure so the cooldown + anti-flap
/// contract is tested without OS calls or a real clock.
///
/// There is deliberately no terminal variant. Every state here is one the
/// machine can leave on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RehomeStep {
    /// Nothing to do (healthy, or not armed).
    Idle,
    /// Fire a rehome attempt now (1-based index).
    Attempt { index: u32 },
    /// Armed but inside the post-attempt cooldown: wait.
    Cooldown { remaining_s: u64 },
    /// The adapter verified healthy for the full reset window: episode reset.
    Recovered,
}

/// Retry + cooldown + anti-flap state machine for one rehome episode.
#[derive(Debug, Clone, Default)]
pub struct RehomeMachine {
    attempts: u32,
    last_attempt: Option<Instant>,
    healthy_since: Option<Instant>,
}

impl RehomeMachine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Undo the most recent attempt (the guard refused it, so it must not be
    /// counted as one). Clears the last-attempt timestamp so a re-evaluation is
    /// not blocked by a phantom cooldown.
    pub fn refund_attempt(&mut self) {
        self.attempts = self.attempts.saturating_sub(1);
        self.last_attempt = None;
    }

    /// One step. `armed` is the trigger level; `verified_healthy` is the
    /// post-rehome good state (high-speed USB AND reception confirmed) read from
    /// the fresh sidecar this tick. `cooldown` is the wait owed after each
    /// attempt; `healthy_reset` is the sustained-healthy window that closes the
    /// episode (anti-flap). Pure.
    ///
    /// `attempts` only ever grows within an episode and is reported, never
    /// compared against a ceiling: the count is diagnostic, not a budget.
    pub fn step(
        &mut self,
        armed: bool,
        verified_healthy: bool,
        cooldown: Duration,
        healthy_reset: Duration,
        now: Instant,
    ) -> RehomeStep {
        if verified_healthy {
            // Track sustained health; only close the episode after the full
            // window, so a flapping adapter cannot reset its attempt count
            // every few seconds and hide how long it has been failing.
            let healthy_since = *self.healthy_since.get_or_insert(now);
            if now.saturating_duration_since(healthy_since) >= healthy_reset {
                let was_mid_episode = self.attempts > 0;
                self.attempts = 0;
                self.last_attempt = None;
                self.healthy_since = None;
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
        // No budget check here. There used to be one, and the state it latched
        // could not clear without the attempts it stopped.
        //
        // Respect the cooldown owed after the most recent attempt: a rehome is
        // a real USB unbind/rebind, so the wait is what keeps a permanent fault
        // from thrashing the bus. It is the same wait on the hundredth attempt
        // as on the second.
        if self.attempts > 0 {
            if let Some(last) = self.last_attempt {
                let elapsed = now.saturating_duration_since(last);
                if elapsed < cooldown {
                    return RehomeStep::Cooldown {
                        remaining_s: (cooldown - elapsed).as_secs(),
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
    cooldown_s: u64,
    before_speed_mbps: Option<u32>,
    after_speed_mbps: Option<u32>,
    reason: Option<&str>,
) -> Fields {
    let mut d = Fields::new();
    d.insert("state".to_string(), MpVal::from(state));
    d.insert("iface".to_string(), MpVal::from(iface));
    d.insert("bind_id".to_string(), MpVal::from(bind_id));
    d.insert("attempt".to_string(), MpVal::from(attempt as u64));
    d.insert("cooldown_s".to_string(), MpVal::from(cooldown_s));
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

    const COOLDOWN: Duration = Duration::from_secs(10);
    const HEALTHY_RESET: Duration = Duration::from_secs(120);

    /// One step with the fixed cooldown and reset window every test uses.
    fn step_at(m: &mut RehomeMachine, armed: bool, healthy: bool, at: Instant) -> RehomeStep {
        m.step(armed, healthy, COOLDOWN, HEALTHY_RESET, at)
    }

    #[test]
    fn fires_one_attempt_then_cools_down() {
        let mut m = RehomeMachine::new();
        let t0 = Instant::now();
        // Armed + faulty + no prior attempt → fire attempt 1 immediately.
        assert_eq!(
            step_at(&mut m, true, false, t0),
            RehomeStep::Attempt { index: 1 }
        );
        // Inside the cooldown: wait.
        match step_at(&mut m, true, false, t0 + Duration::from_secs(3)) {
            RehomeStep::Cooldown { .. } => {}
            other => panic!("expected cooldown, got {other:?}"),
        }
        // After the cooldown: attempt 2.
        assert_eq!(
            step_at(&mut m, true, false, t0 + Duration::from_secs(11)),
            RehomeStep::Attempt { index: 2 }
        );
    }

    #[test]
    fn a_persistent_fault_never_stops_being_retried() {
        // This replaces a test called `budget_caps_to_exhausted_once`, whose
        // comment read "it stays exhausted (no re-loop)". That was the defect,
        // pinned: the budget latched a state whose only exit was a sustained
        // healthy window, the adapter could only become healthy after a
        // successful rehome, and no rehome was attempted while latched. A
        // wedged radio was abandoned after ~90 s until someone rebooted.
        let mut m = RehomeMachine::new();
        let t0 = Instant::now();

        // Walk far past any budget the old code had (3) and keep walking.
        let mut attempts = Vec::new();
        for tick in 0..200u64 {
            let at = t0 + Duration::from_secs(tick * 11);
            if let RehomeStep::Attempt { index } = step_at(&mut m, true, false, at) {
                attempts.push(index);
            }
        }

        assert_eq!(
            attempts.len(),
            200,
            "recovery stopped attempting after {} tries",
            attempts.len()
        );
        // The count keeps climbing so an operator can see how long this has
        // been going on; it is diagnostic, never a ceiling.
        assert_eq!(attempts.first(), Some(&1));
        assert_eq!(attempts.last(), Some(&200));
    }

    #[test]
    fn the_wait_between_attempts_never_grows() {
        // The other half of the old design: an escalating [10, 30, 60] s
        // schedule. On a board recovering its own peripheral there is no herd
        // to spread, so growth only delays recovery.
        let mut m = RehomeMachine::new();
        let t0 = Instant::now();
        for tick in 0..20u64 {
            let fired = t0 + Duration::from_secs(tick * 11);
            assert!(
                matches!(
                    step_at(&mut m, true, false, fired),
                    RehomeStep::Attempt { .. }
                ),
                "attempt {tick} did not fire 11s after the previous one, so the \
                 cooldown grew past the fixed {COOLDOWN:?}"
            );
        }
    }

    #[test]
    fn not_armed_is_idle() {
        let mut m = RehomeMachine::new();
        let t0 = Instant::now();
        assert_eq!(step_at(&mut m, false, false, t0), RehomeStep::Idle);
    }

    #[test]
    fn sustained_health_recovers_and_closes_the_episode() {
        let mut m = RehomeMachine::new();
        let t0 = Instant::now();
        // Spend an attempt.
        step_at(&mut m, true, false, t0);
        // Healthy but not yet for the reset window → Idle, count not reset.
        assert_eq!(
            step_at(&mut m, false, true, t0 + Duration::from_secs(60)),
            RehomeStep::Idle
        );
        // Healthy past the reset window → Recovered + episode closed.
        assert_eq!(
            step_at(&mut m, false, true, t0 + Duration::from_secs(181)),
            RehomeStep::Recovered
        );
        assert_eq!(m.attempts(), 0);
        // A fresh fault attempts again from index 1.
        assert_eq!(
            step_at(&mut m, true, false, t0 + Duration::from_secs(200)),
            RehomeStep::Attempt { index: 1 }
        );
    }

    #[test]
    fn a_long_running_episode_still_recovers_when_the_adapter_comes_back() {
        // The property the latch destroyed. After a hundred failed attempts,
        // sustained health must still close the episode — under the old design
        // this path was unreachable, because reaching health required the
        // attempts the latch had stopped.
        let mut m = RehomeMachine::new();
        let t0 = Instant::now();
        for tick in 0..100u64 {
            step_at(&mut m, true, false, t0 + Duration::from_secs(tick * 11));
        }
        assert_eq!(m.attempts(), 100);
        let healed = t0 + Duration::from_secs(100 * 11);
        step_at(&mut m, false, true, healed);
        assert_eq!(
            step_at(&mut m, false, true, healed + HEALTHY_RESET),
            RehomeStep::Recovered
        );
        assert_eq!(m.attempts(), 0);
    }

    #[test]
    fn a_brief_healthy_blip_does_not_close_the_episode() {
        let mut m = RehomeMachine::new();
        let t0 = Instant::now();
        step_at(&mut m, true, false, t0);
        // Brief health (under the reset window), then faulty again: the healthy
        // timer resets so the next sustained-health window starts fresh, and
        // the attempt count is preserved (not reset by the blip) — otherwise a
        // flapping adapter would hide how long it has been failing.
        step_at(&mut m, false, true, t0 + Duration::from_secs(20));
        assert_eq!(m.attempts(), 1);
        assert_eq!(
            step_at(&mut m, true, false, t0 + Duration::from_secs(40)),
            RehomeStep::Attempt { index: 2 }
        );
    }

    #[test]
    fn a_refunded_attempt_is_not_counted_and_clears_the_cooldown() {
        // The guard can refuse an attempt after the machine authorised it. That
        // must not consume anything, and must not leave a phantom cooldown.
        let mut m = RehomeMachine::new();
        let t0 = Instant::now();
        assert_eq!(
            step_at(&mut m, true, false, t0),
            RehomeStep::Attempt { index: 1 }
        );
        m.refund_attempt();
        assert_eq!(m.attempts(), 0);
        assert_eq!(
            step_at(&mut m, true, false, t0 + Duration::from_secs(1)),
            RehomeStep::Attempt { index: 1 }
        );
    }

    #[test]
    fn detail_is_bland_and_omits_absent_fields() {
        let d = usb_rehome_detail("rehoming", "wlan1", "1-1", 1, 60, Some(12), None, None);
        assert_eq!(d.get("state").and_then(|v| v.as_str()), Some("rehoming"));
        assert_eq!(d.get("bind_id").and_then(|v| v.as_str()), Some("1-1"));
        assert_eq!(d.get("cooldown_s").and_then(|v| v.as_u64()), Some(60));
        assert_eq!(
            d.get("before_usb_speed_mbps").and_then(|v| v.as_u64()),
            Some(12)
        );
        assert!(!d.contains_key("after_usb_speed_mbps"));
        assert!(!d.contains_key("reason"));
        // The old field named a budget that no longer exists.
        assert!(!d.contains_key("max_attempts"));
        let g = usb_rehome_detail(
            "guard_blocked",
            "wlan1",
            "1-1",
            0,
            60,
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
