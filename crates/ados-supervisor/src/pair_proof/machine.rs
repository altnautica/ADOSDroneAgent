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

/// Re-arm episodes allowed per key fingerprint, ever. Survives reboots (the
/// record is persistent), so a rig that cannot bind does not spend its life
/// re-binding.
pub const DEFAULT_MAX_REARM_EPISODES: u32 = 5;

/// Wall-clock wait between re-arm episodes, anchored on the persisted
/// `last_rearm_at` rather than an in-process timer, so a restart (or a crash
/// loop) cannot shorten it.
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
    /// Every episode for this key is spent and the fault persists.
    Exhausted,
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
    /// Episodes already spent on this fingerprint.
    pub episodes: u32,
    pub max_episodes: u32,
    /// Wall clock of the last episode, from the persisted record.
    pub last_rearm_at: Option<u64>,
    pub cooldown_s: u64,
    pub now_unix: u64,
}

/// The one rule: re-arm only a key that has never once been confirmed to work.
///
/// Precedence matters. The operator force runs first because it exists to
/// override exactly this machine. The proven latch runs next, ahead of the
/// hold, the budget and the cooldown, so no combination of inputs can re-bind a
/// pair that has worked — that path is closed by construction rather than by
/// every later branch happening to decline.
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
    if input.episodes >= input.max_episodes {
        return RearmStep::Exhausted;
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
mod tests {
    use super::*;
    use std::time::Instant;

    fn base() -> RearmInput {
        RearmInput {
            proven: false,
            forced: false,
            hold_armed: true,
            episodes: 0,
            max_episodes: DEFAULT_MAX_REARM_EPISODES,
            last_rearm_at: None,
            cooldown_s: DEFAULT_REARM_COOLDOWN_S,
            now_unix: 1_000_000,
        }
    }

    // ── the headline rule ─────────────────────────────────────────────────────

    #[test]
    fn a_proven_key_is_never_rearmed_however_long_it_is_down() {
        // The property that matters most: once a fingerprint is latched proven,
        // no combination of a held unproven condition, spare budget and elapsed
        // cooldown re-opens a bind window. A multi-day RF dropout on a healthy
        // pair must be structurally incapable of triggering a re-bind.
        let mut input = RearmInput {
            proven: true,
            ..base()
        };
        let day = 24 * 3600;
        for d in 0..30 {
            input.now_unix = 1_000_000 + d * day;
            assert_eq!(
                decide_rearm(input),
                RearmStep::Proven,
                "day {d} must not re-arm a proven key"
            );
        }
    }

    #[test]
    fn one_proof_locks_out_every_later_rearm_for_that_key() {
        // Unproven with budget → arms. The same inputs once proven → never again.
        let unproven = base();
        assert!(matches!(decide_rearm(unproven), RearmStep::Arm { .. }));
        let proven = RearmInput {
            proven: true,
            ..unproven
        };
        assert_eq!(decide_rearm(proven), RearmStep::Proven);
    }

    // ── arming ────────────────────────────────────────────────────────────────

    #[test]
    fn a_never_proven_key_arms_once_the_hold_is_served() {
        assert_eq!(
            decide_rearm(RearmInput {
                hold_armed: false,
                ..base()
            }),
            RearmStep::Idle
        );
        assert_eq!(
            decide_rearm(base()),
            RearmStep::Arm {
                episode: 1,
                forced: false
            }
        );
    }

    #[test]
    fn the_episode_index_follows_the_persisted_count() {
        assert_eq!(
            decide_rearm(RearmInput {
                episodes: 3,
                last_rearm_at: None,
                ..base()
            }),
            RearmStep::Arm {
                episode: 4,
                forced: false
            }
        );
    }

    // ── budget + cooldown ─────────────────────────────────────────────────────

    #[test]
    fn the_episode_budget_is_bounded() {
        for spent in 0..DEFAULT_MAX_REARM_EPISODES {
            assert!(
                matches!(
                    decide_rearm(RearmInput {
                        episodes: spent,
                        ..base()
                    }),
                    RearmStep::Arm { .. }
                ),
                "{spent} spent should still arm"
            );
        }
        assert_eq!(
            decide_rearm(RearmInput {
                episodes: DEFAULT_MAX_REARM_EPISODES,
                ..base()
            }),
            RearmStep::Exhausted
        );
        assert_eq!(
            decide_rearm(RearmInput {
                episodes: DEFAULT_MAX_REARM_EPISODES + 7,
                ..base()
            }),
            RearmStep::Exhausted
        );
    }

    #[test]
    fn the_cooldown_is_wall_clock_so_a_reboot_cannot_shorten_it() {
        // The anchor is the persisted last-episode timestamp, not an in-process
        // timer, so a restart mid-cooldown resumes where it was rather than
        // starting a fresh window (which a crash loop would exploit).
        let last = 1_000_000u64;
        let mid = RearmInput {
            episodes: 1,
            last_rearm_at: Some(last),
            now_unix: last + DEFAULT_REARM_COOLDOWN_S - 1,
            ..base()
        };
        assert_eq!(decide_rearm(mid), RearmStep::Cooldown { remaining_s: 1 });
        // A brand-new process with no memory reads the same record and gets the
        // same answer — the decision has no in-process state at all.
        assert_eq!(decide_rearm(mid), RearmStep::Cooldown { remaining_s: 1 });
        // Once the wall clock passes it, the next episode is allowed.
        assert_eq!(
            decide_rearm(RearmInput {
                now_unix: last + DEFAULT_REARM_COOLDOWN_S,
                ..mid
            }),
            RearmStep::Arm {
                episode: 2,
                forced: false
            }
        );
    }

    #[test]
    fn a_backwards_clock_does_not_underflow_the_cooldown() {
        let last = 1_000_000u64;
        assert_eq!(
            decide_rearm(RearmInput {
                episodes: 1,
                last_rearm_at: Some(last),
                now_unix: 5,
                ..base()
            }),
            RearmStep::Cooldown {
                remaining_s: last + DEFAULT_REARM_COOLDOWN_S - 5
            }
        );
    }

    // ── the operator escape hatch ─────────────────────────────────────────────

    #[test]
    fn force_overrides_the_proven_latch_the_budget_and_the_cooldown() {
        let blocked = RearmInput {
            proven: true,
            hold_armed: false,
            episodes: DEFAULT_MAX_REARM_EPISODES + 3,
            last_rearm_at: Some(1_000_000),
            ..base()
        };
        assert_eq!(decide_rearm(blocked), RearmStep::Proven);
        assert_eq!(
            decide_rearm(RearmInput {
                forced: true,
                ..blocked
            }),
            RearmStep::Arm {
                episode: DEFAULT_MAX_REARM_EPISODES + 4,
                forced: true
            }
        );
    }

    // ── signal derivation ─────────────────────────────────────────────────────

    #[test]
    fn the_drone_unproven_signal_requires_a_live_transmitter() {
        // rf_unverified means "transmitting, zero confirmed reception" — an idle
        // or absent radio reports null and must accumulate nothing.
        assert!(drone_signals(Some(true), Some(false)).unproven);
        assert!(!drone_signals(None, None).unproven);
        assert!(!drone_signals(Some(false), Some(false)).unproven);
        // A locked channel is proof, and it also clears any same-tick unproven
        // claim rather than letting the two disagree.
        assert!(drone_signals(Some(false), Some(true)).proven);
        let both = drone_signals(Some(true), Some(true));
        assert!(both.proven && !both.unproven);
    }

    #[test]
    fn the_ground_station_counts_searching_but_not_the_blocked_states() {
        // The receive plane has no transmit-side verdict, so `searching` (key
        // present, chain running, nothing decoding) is its unproven signal.
        assert!(gs_signals("searching", 0).unproven);
        // These three mean the chain never ran, so there is no verdict on the
        // key at all — re-arming on them would re-arm on a missing antenna.
        for state in ["blocked_unpaired", "reg_blocked", "no_injection", "active"] {
            assert!(
                !gs_signals(state, 0).unproven,
                "{state} is not a verdict on the key"
            );
        }
        // A decoded packet is proof, and outranks the state string.
        assert!(gs_signals("active", 12).proven);
        let searching_but_decoding = gs_signals("searching", 3);
        assert!(searching_but_decoding.proven && !searching_but_decoding.unproven);
    }

    // ── the debounce ──────────────────────────────────────────────────────────

    #[test]
    fn a_transient_gap_under_the_hold_does_not_arm_and_a_fresh_onset_restarts_it() {
        let hold = REARM_CONFIRM_HOLD;
        let mut t = HoldTrigger::with_hold(hold);
        let t0 = Instant::now();
        assert!(!t.observe(true, t0));
        assert!(!t.observe(true, t0 + hold - Duration::from_secs(1)));
        // Released one second short: nothing armed, and the clock restarts.
        assert!(!t.observe(false, t0 + hold));
        let t1 = t0 + hold + Duration::from_secs(1);
        assert!(!t.observe(true, t1));
        // The FULL hold must be served again from the fresh onset.
        assert!(!t.observe(true, t1 + hold - Duration::from_secs(1)));
        assert!(t.observe(true, t1 + hold));
    }

    #[test]
    fn the_hold_is_long_enough_that_a_radio_coming_up_cannot_trip_it() {
        // Re-arming opens a bind window on a rig that believes it is paired, so
        // the bar is a plainly persistent fault, not a slow start.
        assert!(
            REARM_CONFIRM_HOLD >= Duration::from_secs(300),
            "confirm hold must stay long, got {:?}",
            REARM_CONFIRM_HOLD
        );
    }
}
