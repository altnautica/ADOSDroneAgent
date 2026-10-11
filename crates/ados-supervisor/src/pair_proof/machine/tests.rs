use super::*;
use std::time::Instant;

fn base() -> RearmInput {
    RearmInput {
        proven: false,
        forced: false,
        hold_armed: true,
        episodes: 0,
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

// ── cooldown ──────────────────────────────────────────────────────────────

#[test]
fn a_fault_that_persists_keeps_being_retried_one_episode_per_cooldown() {
    // There is no budget: the hundredth episode arms on the same terms as
    // the second. A key from a peer that was reflashed yesterday is still
    // worth re-arming today.
    for spent in [1u32, 5, 6, 100] {
        let last = 1_000_000u64;
        assert_eq!(
            decide_rearm(RearmInput {
                episodes: spent,
                last_rearm_at: Some(last),
                now_unix: last + DEFAULT_REARM_COOLDOWN_S,
                ..base()
            }),
            RearmStep::Arm {
                episode: spent + 1,
                forced: false
            },
            "{spent} spent must still arm once the cooldown has passed"
        );
    }
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
fn force_overrides_the_proven_latch_and_the_cooldown() {
    let blocked = RearmInput {
        proven: true,
        hold_armed: false,
        episodes: 8,
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
            episode: 9,
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
