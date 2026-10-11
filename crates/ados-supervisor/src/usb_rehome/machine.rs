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
mod tests;
