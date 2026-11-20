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
//! enter, a held detour round a trunk and a seeded random stand. Latency,
//! stick feel and throughput stay with the real-time World Engine benches.
//! The shaper pins ([`shaper_pins`]) hold the trajectories of goals, holds
//! and landings to their recordings.
//!
//! Every test name starts `dst_`: `cargo test -p ados-pilot dst` runs them.

mod fc;
mod forest;
#[path = "../pins/mod.rs"]
mod pins;
mod rng;
mod scenarios;
mod shaper_pins;
mod sim;

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
