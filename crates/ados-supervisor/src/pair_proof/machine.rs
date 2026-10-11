//! Pure decision logic for the auto-pair re-arm latch. No OS calls, no clock —
//! every input is injected, so the whole contract is unit-tested on any host.
//!
//! Two pieces:
//!
//! - [`RearmSignals`], derived from the radio's own `wfb-stats.json`, answering
//!   "does this rig currently look like it holds a key that does not work?" and
//!   "has this key just been seen working?".
//! - [`decide_rearm`], which turns those plus the persisted record into one of a
//!   small set of outcomes.
//!
//! The debounce is deliberately the same [`HoldTrigger`] the USB-rehome self-heal
//! uses: a level-triggered confirm window that resets the instant the condition
//! releases, so a fresh onset must serve the full hold again.

use std::time::Duration;

/// The event kind recorded for a re-arm decision.
pub const PAIR_REARM_KIND: &str = "wfb.pair.rearm";

/// The USB-rehome trigger, reused verbatim. Its contract is exactly what the
/// re-arm needs — hold a condition continuously for a window, report a level
/// rather than an edge, reset on release — and a second copy of a debounce is a
/// second thing to keep correct.
pub use crate::usb_rehome::machine::RehomeTrigger as HoldTrigger;

/// How long the "this key does not work" condition must hold continuously before
/// a re-arm is authorized. Long: re-arming opens a bind window on a rig that
/// believes it is paired, so the bar is a fault that has plainly persisted, not
/// a radio that is still coming up.
pub const REARM_CONFIRM_HOLD: Duration = Duration::from_secs(600);

/// Wall-clock wait between re-arm episodes, anchored on the persisted
/// `last_rearm_at` rather than an in-process timer, so a restart (or a crash
/// loop) cannot shorten it.
///
/// This is the only bound. There is no episode budget: a budget latched a
/// permanent "exhausted" verdict per key that survived reboots, so a drone
/// holding a key from a reflashed peer stopped trying after a couple of hours
/// and needed someone to reach it before it would ever bind again. A fixed
/// cooldown keeps recovery going at a pace that cannot become a bind storm.
pub const DEFAULT_REARM_COOLDOWN_S: u64 = 1800;

/// Max age of `wfb-stats.json` before its signals count as no signal at all. The
/// radio rewrites it every ~1 s; older than this means the writer is stopped or
/// crashed and its last flags are frozen. Acting on a frozen `rf_unverified`
/// would arm against a radio that is not even running.
pub const STATS_FRESH_CEILING: Duration = Duration::from_secs(30);

/// What the radio's sidecar says about this key, reduced to the two questions
/// the latch asks. Never both true.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RearmSignals {
    /// The key is in use and is demonstrably not working.
    pub unproven: bool,
    /// The key has just been observed carrying a link.
    pub proven: bool,
}

/// Derive the drone-side signals.
///
/// Unproven is `rf_unverified`, NOT `!channel_locked`. `rf_unverified` requires
/// the transmit counter to be advancing, so an idle radio, an unplugged dongle
/// or a radio that never started reads as no signal rather than accumulating
/// hold toward a re-bind. Proven is `channel_locked`: a locked channel means
/// frames from the peer decoded, which only this key could have done.
pub fn drone_signals(rf_unverified: Option<bool>, channel_locked: Option<bool>) -> RearmSignals {
    let proven = channel_locked == Some(true);
    RearmSignals {
        // A locked channel wins: a link that is up is not an unproven key,
        // whatever a same-tick transmit-side verdict says.
        unproven: !proven && rf_unverified == Some(true),
        proven,
    }
}

/// Derive the ground-station signals.
///
/// The receive plane never measures a transmit path, so its sidecar carries
/// `rf_unverified: null` by design and the drone's rule does not transfer. Its
/// equivalent of "holding a key that does not work" is `searching`: the key is
/// present, the receive chain is running, and nothing decodes.
///
/// The other blocked states are deliberately NOT unproven. `blocked_unpaired`,
/// `reg_blocked` and `no_injection` all mean the receive chain never ran, so
/// there is no verdict on the key — re-arming on them would be re-arming on a
/// missing antenna or a regulatory refusal.
///
/// Proven is a decoded packet: `packets_received > 0`.
pub fn gs_signals(state: &str, packets_received: u64) -> RearmSignals {
    let proven = packets_received > 0;
    RearmSignals {
        unproven: !proven && state == "searching",
        proven,
    }
}

/// What the latch decided this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RearmStep {
    /// Nothing to do.
    Idle,
    /// This key has been confirmed to work at least once. It is never re-armed
    /// again, however long it has been down. Distinct from [`RearmStep::Idle`]
    /// so a log says why nothing happened.
    Proven,
    /// Open a bind window now.
    Arm {
        /// 1-based episode index this arm consumes.
        episode: u32,
        /// True when an operator asked for it rather than the latch deciding.
        forced: bool,
    },
    /// The condition holds but the wall-clock cooldown from the last episode has
    /// not elapsed.
    Cooldown { remaining_s: u64 },
}

/// Everything [`decide_rearm`] needs, read once by the caller.
#[derive(Debug, Clone, Copy)]
pub struct RearmInput {
    /// The key's fingerprint has been confirmed working at least once.
    pub proven: bool,
    /// An operator asked for one re-arm regardless of the latch.
    pub forced: bool,
    /// The unproven condition has held for the full confirm window.
    pub hold_armed: bool,
    pub episodes: u32,
    /// Wall clock of the last episode, from the persisted record.
    pub last_rearm_at: Option<u64>,
    pub cooldown_s: u64,
    pub now_unix: u64,
}

/// The one rule: re-arm only a key that has never once been confirmed to work.
///
/// Precedence matters. The operator force runs first because it exists to
/// override exactly this machine. The proven latch runs next, ahead of the
/// hold and the cooldown, so no combination of inputs can re-bind a pair that
/// has worked — that path is closed by construction rather than by every later
/// branch happening to decline. A never-proven key is re-armed for as long as
/// the fault persists, one episode per cooldown.
pub fn decide_rearm(input: RearmInput) -> RearmStep {
    if input.forced {
        return RearmStep::Arm {
            episode: input.episodes.saturating_add(1),
            forced: true,
        };
    }
    if input.proven {
        return RearmStep::Proven;
    }
    if !input.hold_armed {
        return RearmStep::Idle;
    }
    if let Some(last) = input.last_rearm_at {
        let ready_at = last.saturating_add(input.cooldown_s);
        if input.now_unix < ready_at {
            return RearmStep::Cooldown {
                remaining_s: ready_at - input.now_unix,
            };
        }
    }
    RearmStep::Arm {
        episode: input.episodes.saturating_add(1),
        forced: false,
    }
}

#[cfg(test)]
mod tests;
