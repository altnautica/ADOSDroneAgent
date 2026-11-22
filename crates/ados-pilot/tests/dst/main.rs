//! In-process deterministic simulation (DST) of the pilot core.
//!
//! The pilot's decision logic ([`ados_pilot::core::Pilot`]) is a pure state
//! machine with time passed in, so it can be flown without sockets or a
//! clock: these tests step it in 20 ms ticks on one thread against a
//! simulated ArduPilot copter ([`fc`]), an operator surface and the router's
//! mission state ([`sim`]), thousands of times faster than real time. Every
//! source of variation (wind, sensor noise, battery, airframe, route) comes
//! from one seeded generator ([`rng`]), so a seed names a run exactly: the
//! meta-tests check that one seed replays byte for byte and that seeds
//! differ.
//!
//! The matrix ([`scenarios`]) covers decision sequences, not flight feel:
//! the low-battery floor's countdown and SmartRTL return, the supervised
//! pause, resume and stalled-waypoint skip; and follow flights through
//! trunks ([`forest`]): a corridor between trunk rows, a gap too narrow to
//! enter, a held detour round a trunk, a seeded random stand and the same
//! stand under a canopy of crowns, scanned by a lidar the copter's lean
//! tilts; and FC
//! position resets ([`pos_reset`]): a 21 m EKF step (with a height step)
//! under the pilot's hold and under a GUIDED target nobody streams, a GPS
//! glitch's smaller step, and fast flight, climbs and a stepped clock that
//! are none. Latency, stick feel and throughput stay with the real-time
//! World Engine benches.
//! The shaper pins ([`shaper_pins`]) hold the trajectories of goals, holds
//! and landings to their recordings.
//!
//! Every test name starts `dst_`: `cargo test -p ados-pilot dst` runs them.

mod bench;
mod fc;
mod fc_avoid;
mod follow;
mod forest;
mod goto_wall;
mod lost_hold;
mod metrics;
mod mine;
mod mission_route;
#[path = "../pins/mod.rs"]
mod pins;
mod pos_reset;
mod rng;
mod rth_wall;
mod scenarios;
mod sensors;
mod shaper_pins;
mod sim;
mod unseen;
mod voxmap;

use std::time::Instant;

use scenarios::ALL;

/// Seeds flown per scenario in the matrix.
const SEEDS: std::ops::RangeInclusive<u64> = 1..=4;

/// Fly `name` on every matrix seed; print simulated against wall time.
fn matrix(name: &str) {
    let (_, fly) = ALL
        .iter()
        .find(|(n, _)| *n == name)
        .expect("a scenario of that name");
    let wall = Instant::now();
    let mut sim_s = 0.0;
    for seed in SEEDS {
        let s = fly(seed);
        // The FC's proximity backstop never acts while the pilot flies.
        assert_eq!(
            s.fc.prox.trips, 0,
            "{name} s{seed}: the FC's avoidance acted"
        );
        sim_s += s.sim_s();
    }
    let wall_s = wall.elapsed().as_secs_f64();
    eprintln!(
        "dst {name}: {} seeds, {sim_s:.1} sim s in {wall_s:.2} wall s ({:.0}x real time)",
        SEEDS.count(),
        sim_s / wall_s
    );
}

#[test]
fn dst_low_battery_floor_returns_by_smart_rtl() {
    matrix("low_battery");
}

#[test]
fn dst_energy_return_lands_before_the_critical_level() {
    matrix("energy_return");
}

#[test]
fn dst_energy_countdown_withdrawn_then_counts_down_again() {
    matrix("energy_withdrawn");
}

#[test]
fn dst_supervised_liftoff_reaches_hover() {
    matrix("liftoff");
}

#[test]
fn dst_link_loss_returns_by_smart_rtl() {
    matrix("link_loss");
}

#[test]
fn dst_mission_pause_resume_and_stall_skip() {
    matrix("mission");
}

#[test]
fn dst_forest_corridor_flies_full_speed_between_trunk_rows() {
    matrix("forest_corridor");
}

#[test]
fn dst_forest_gap_narrower_than_the_margin_is_never_entered() {
    matrix("forest_gap");
}

#[test]
fn dst_forest_detour_round_a_trunk_is_held() {
    matrix("forest_detour");
}

#[test]
fn dst_forest_stand_follows_a_walker_inside_the_margin() {
    matrix("forest_stand");
}

#[test]
fn dst_forest_canopy_follows_a_walker_under_the_crowns() {
    matrix("forest_canopy");
}

/// A 21 m EKF position reset in the pilot's hold: the copter holds where it
/// is, never flying the step to the moved GUIDED target.
#[test]
fn dst_position_reset_in_the_pilots_hold_holds_in_place() {
    matrix("reset_hold");
}

/// The incident: a 21 m reset under a GUIDED target a ground station left,
/// with nobody streaming: the pilot's zero velocity holds the copter.
#[test]
fn dst_position_reset_under_an_unowned_guided_target_holds_in_place() {
    matrix("reset_idle");
}

#[test]
fn dst_gps_glitch_reset_is_held() {
    matrix("reset_glitch");
}

#[test]
fn dst_fast_flight_and_a_stepped_clock_are_no_reset() {
    matrix("no_false_reset");
}

/// The simulated FC reproduces the dash when nothing answers the reset.
#[test]
fn dst_without_the_pilot_the_fc_flies_the_reset() {
    for seed in SEEDS {
        pos_reset::fc_flies_the_reset_alone(seed);
    }
}

/// The stand whose follow route passed a trunk beyond the margin but inside
/// the brake's keep-out, where the vehicle crawled until the planner chose
/// again round it.
#[test]
fn dst_forest_stand_chooses_again_where_it_crawls() {
    forest::forest_stand(14);
}

/// Seeds every follow-intent case flies.
const FOLLOW_SEEDS: std::ops::RangeInclusive<u64> = 1..=4;

/// The least share of a walk the vehicle sees the subject through
/// occluding trunks and round a corner.
const FOLLOW_VISIBILITY: f32 = 0.85;

/// Every follow run: out of the trunks' margin, and the subject's
/// separation kept (3 m unless 3.4 m over its head).
fn follow_safe(name: &str, seed: u64, f: &follow::Follow) {
    assert!(
        f.min_clearance_m >= 1.5,
        "{name} s{seed}: {:.2} m from a trunk",
        f.min_clearance_m
    );
    assert!(
        f.min_separation_m >= follow::SEPARATION_M,
        "{name} s{seed}: {:.2} m from the subject",
        f.min_separation_m
    );
}

/// One occluder case on every seed, flown with intents and with today's
/// ring goal: with intents the vehicle sees the subject at least
/// [`FOLLOW_VISIBILITY`] of every walk, and more of them all than the ring
/// goal does.
fn follow_occluded(name: &str, case: fn(u64, bool) -> (sim::Sim, follow::Follow)) {
    let (mut ring, mut intent) = (0.0, 0.0);
    for seed in FOLLOW_SEEDS {
        let (_, r) = case(seed, true);
        let (_, f) = case(seed, false);
        eprintln!(
            "{name} s{seed}: visibility ring {:.2}, intent {:.2}",
            r.visibility(),
            f.visibility()
        );
        follow_safe(name, seed, &f);
        assert!(
            f.visibility() >= FOLLOW_VISIBILITY,
            "{name} s{seed}: saw the subject {:.2} of the walk",
            f.visibility()
        );
        ring += r.visibility();
        intent += f.visibility();
    }
    assert!(
        intent > ring,
        "{name}: intents saw {intent:.2}, the ring {ring:.2}"
    );
}

/// Trunks on the walker's way pass between it and a vehicle behind.
#[test]
fn dst_follow_intent_keeps_the_subject_in_sight_past_trunks() {
    follow_occluded("trunks", follow::trunks);
}

/// A walker turning round the end of a wall beside its way.
#[test]
fn dst_follow_intent_keeps_the_subject_in_sight_round_a_wall_corner() {
    follow_occluded("corner", follow::corner);
}

/// A walker turning out of a corridor at a T.
#[test]
fn dst_follow_intent_keeps_the_subject_in_sight_round_a_t_junction() {
    follow_occluded("t_junction", follow::t_junction);
}

/// The camera loses a walker just after it turned back toward the
/// vehicle: the separation holds while it walks on unseen.
#[test]
fn dst_follow_intent_keeps_its_separation_from_a_subject_lost_walking_toward_it() {
    for seed in FOLLOW_SEEDS {
        let (_, f) = follow::toward_at_loss(seed, false);
        follow_safe("toward_at_loss", seed, &f);
    }
}

/// A skill that lost its subject holds, naming none, while the subject
/// walks (and runs) back under the vehicle, boxed by the camera with no
/// world position, late and with frames lost, and never returned by the
/// level planar lidar: the vehicle keeps the subject's separation.
#[test]
fn dst_a_held_vehicle_keeps_clear_of_a_person_walking_back_under_it() {
    for seed in FOLLOW_SEEDS {
        for pace in [follow::WALK_MPS, follow::JOG_MPS] {
            let (_, f) = lost_hold::walk_under(seed, pace);
            follow_safe("lost_hold", seed, &f);
        }
    }
}

/// A ring goal names its subject by place alone, read from the subject's
/// own box a quarter long, while the pilot places the same box as a person
/// track: the person keep never binds on the vehicle's own subject, and the
/// subject brake keeps the real subject (its camera track, nearer than the
/// long `subject_ne`) at least its kept distance off.
#[test]
fn dst_a_ring_goal_never_keeps_the_person_distance_from_its_own_subject() {
    for seed in FOLLOW_SEEDS {
        for pace in [follow::WALK_MPS, follow::JOG_MPS] {
            let (_, f, bound) = lost_hold::ring_own_box(seed, pace);
            assert!(f.seen > 0, "ring_own_box s{seed}: the camera boxed nothing");
            assert_eq!(
                bound, 0,
                "ring_own_box s{seed} pace {pace}: person keep bound"
            );
            // The subject brake's kept distance for the goal's 0.5 m sigma:
            // body 0.4 + vehicle 0.4 + margin 0.5 + two sigma.
            assert!(
                f.min_separation_m >= 2.3,
                "ring_own_box s{seed} pace {pace}: closed to {:.2} m of the subject",
                f.min_separation_m
            );
        }
    }
}

/// A walker straight away through a stand: the vehicle keeps within the
/// band's far edge plus 2 m for 0.8 of the walk. The jog is measured by
/// [`dst_follow_table`].
#[test]
fn dst_follow_intent_keeps_up_with_a_subject_walking_away() {
    for seed in FOLLOW_SEEDS {
        let (_, f) = follow::walk_away(seed, follow::WALK_MPS, sensors::SensorTier::Planar, false);
        follow_safe("walk_away", seed, &f);
        assert!(
            f.reach_share() >= 0.8,
            "walk_away s{seed}: within reach {:.2} of the walk, {:.1} m at most",
            f.reach_share(),
            f.max_dist_m
        );
    }
}

/// An orbit intent (its bearing the orbit's phase over the ground) round
/// a standing subject among trunks: the vehicle goes all the way round
/// within 1.5 times the orbit's own 36 s, keeping its separation and the
/// trunks' margin.
#[test]
fn dst_follow_intent_orbits_a_standing_subject_all_the_way_round() {
    for seed in FOLLOW_SEEDS {
        let (_, f) = follow::orbit(seed);
        follow_safe("orbit", seed, &f);
        let full = f
            .orbit_full_ms
            .unwrap_or_else(|| panic!("orbit s{seed}: went {:.0}° round", f.orbit_deg));
        assert!(
            full <= 54_000,
            "orbit s{seed}: all the way round at {full} ms"
        );
    }
}

/// Every follow case with the ring goal and with intents, both on seeds
/// 1-4, as a table: visibility, separation, clearance, within-band share.
#[test]
#[ignore = "the follow table: minutes; run with --ignored --nocapture follow_table"]
fn dst_follow_table() {
    follow::table();
}

/// One seed replays byte for byte: every frame the pilot sent, every frame
/// it published and the vehicle's state each tick.
#[test]
fn dst_same_seed_replays_byte_identical() {
    for (name, fly) in ALL {
        let a = fly(7);
        let b = fly(7);
        if let Some((line, x, y)) = a.trace.first_difference(&b.trace) {
            panic!("{name}: seed 7 diverged at trace line {line}:\n{x}\n{y}");
        }
        assert_eq!(a.trace.hash(), b.trace.hash(), "{name}");
        assert!(a.trace.bytes.len() > 10_000, "{name}: a trace was written");
    }
}

/// Different seeds fly different runs.
#[test]
fn dst_different_seeds_differ() {
    for (name, fly) in ALL {
        let a = fly(11).trace.hash();
        let b = fly(12).trace.hash();
        assert_ne!(a, b, "{name}: seeds 11 and 12 gave one trace");
    }
}
