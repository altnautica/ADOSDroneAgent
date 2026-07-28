//! The four flight-gate scenarios as assertions.
//!
//! First flight is gated on software-in-the-loop, with a real autopilot per
//! aircraft. These run the same four scenarios against the control laws
//! directly, with no autopilot required: same 10 Hz cadence, same 2 Hz beacons
//! with wire quantisation and dead reckoning, first-order velocity plant.
//!
//! What that buys and what it does not: it falsifies a control-law bug — every
//! one of these caught a real one during development — and it says nothing about
//! the autopilot's attitude loop, its EKF, or its failsafes. It is NOT the SITL
//! gate and does not substitute for it.
//!
//! `cargo run -p ados-swarm-control --example swarm_scenarios` prints the same
//! runs with their measured numbers.

use ados_swarm_control::scenarios;
use ados_swarm_control::separation::SEPARATION_HARD_M;
use ados_swarm_control::NEIGHBOR_STALE;

#[test]
fn scenario_1_two_drones_on_a_collision_course_stay_apart_and_the_lower_slot_climbs() {
    let s = scenarios::separation_collision_course();
    assert!(
        (s.closing_speed_mps - 4.0).abs() < 1e-9 && (s.start_separation_m - 20.0).abs() < 1e-9,
        "the scenario must be the plan's: {s:?}"
    );
    assert!(
        s.min_separation_m > SEPARATION_HARD_M,
        "minimum separation {:.3} m breached the {SEPARATION_HARD_M} m floor",
        s.min_separation_m
    );
    assert!(
        s.climb_slot_1_m > 0.25,
        "the lower-slot drone must climb, got {:.3} m",
        s.climb_slot_1_m
    );
    assert!(
        s.hard_engaged,
        "a sustained collision course must engage the hard override"
    );
    // The climb is a deconfliction step, not an ascent: the ratchet bug this
    // pins took the pair almost forty metres up over the same ninety seconds.
    assert!(
        s.climb_slot_1_m < 5.0 && s.climb_slot_2_m < 5.0,
        "climb must be bounded, got {:.1} / {:.1} m",
        s.climb_slot_1_m,
        s.climb_slot_2_m
    );
}

#[test]
fn scenario_2_eight_drones_flock_to_a_target_without_breaching_the_floor() {
    let f = scenarios::flocking_to_target();
    assert!(
        f.start_min_pairwise_m >= SEPARATION_HARD_M,
        "the run must START legal or it measures the initial condition: {f:?}"
    );
    assert_eq!(
        f.arrived, f.fleet,
        "all {} drones must arrive within 30 m; worst error {:.1} m",
        f.fleet, f.max_target_error_m
    );
    assert!(
        f.min_pairwise_m > SEPARATION_HARD_M,
        "closest pair over the whole run was {:.3} m",
        f.min_pairwise_m
    );
}

#[test]
fn scenario_3_a_wedge_settles_inside_three_metres_of_station() {
    let w = scenarios::formation_wedge();
    assert!(
        w.max_offset_error_m < 3.0,
        "worst steady-state station error {:.3} m",
        w.max_offset_error_m
    );
    assert!(
        w.min_pairwise_m > SEPARATION_HARD_M,
        "closest pair over the whole run was {:.3} m",
        w.min_pairwise_m
    );
    // A formation at 12 m spacing must settle ABOVE the soft separation radius,
    // or the safety layer would be permanently active on a nominal formation.
    assert!(
        w.min_pairwise_m > 5.0,
        "a formed wedge should not sit inside the safety layer: {:.3} m",
        w.min_pairwise_m
    );
}

#[test]
fn scenario_4_a_lost_swarm_bus_drops_out_and_stops_flying() {
    let b = scenarios::beacon_loss();
    assert_eq!(
        b.survivor_neighbours_before, 2,
        "three drones means two neighbours each before the loss"
    );
    assert_eq!(
        b.survivor_neighbours_after,
        b.survivor_neighbours_before - 1,
        "the survivors must DROP the silent drone, not keep it forever: {b:?}"
    );
    assert!(
        b.drop_delay_s <= NEIGHBOR_STALE.as_secs_f64() + 0.2,
        "dropped after {:.1} s, staleness window is {:.1} s",
        b.drop_delay_s,
        NEIGHBOR_STALE.as_secs_f64()
    );
    assert!(
        b.survivors_still_emitting,
        "the survivors must carry on flying: {b:?}"
    );
    assert_eq!(
        b.isolated_emissions_after_stale, 0,
        "the isolated drone must emit NOTHING once its table has gone stale"
    );
    assert!(
        b.isolated_drift_m < 0.5,
        "the isolated drone must hold, drifted {:.3} m",
        b.isolated_drift_m
    );
}

#[test]
fn the_scenarios_are_deterministic() {
    // A scenario that only passes sometimes is not a gate. Every run is seeded and
    // clock-free, so two runs must agree exactly.
    assert_eq!(
        scenarios::separation_collision_course(),
        scenarios::separation_collision_course()
    );
    assert_eq!(scenarios::formation_wedge(), scenarios::formation_wedge());
    assert_eq!(scenarios::beacon_loss(), scenarios::beacon_loss());
    assert_eq!(
        scenarios::flocking_to_target(),
        scenarios::flocking_to_target()
    );
}
