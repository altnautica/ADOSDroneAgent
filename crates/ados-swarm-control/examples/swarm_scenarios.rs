//! Runnable harness for the four flight-gate scenarios.
//!
//! ```text
//! cargo run -p ados-swarm-control --example swarm_scenarios
//! ```
//!
//! Prints the measured numbers and the pass/fail verdict against the plan's
//! criteria, and exits non-zero if any criterion fails, so it can be wired into a
//! bench script. `tests/scenarios.rs` asserts the same criteria as ordinary unit
//! tests; this exists so the numbers can be read without a test harness.
//!
//! This is the CONTROL-LAW gate, not the SITL gate. The plan requires Atlas Sim
//! Bench with N SITL instances before any flight; that bench is not in this
//! repository, so this reproduces the scenarios against a first-order velocity
//! plant instead of ArduPilot.

use ados_swarm_control::scenarios;
use ados_swarm_control::separation::SEPARATION_HARD_M;

fn verdict(pass: bool) -> &'static str {
    if pass {
        "PASS"
    } else {
        "FAIL"
    }
}

fn main() {
    let mut all = true;

    println!("ADOS swarm autonomy — control-law scenario gate");
    println!("(not a substitute for SITL with a real autopilot; see module docs)\n");

    // --- 1 ---------------------------------------------------------------
    let s = scenarios::separation_collision_course();
    let pass = s.min_separation_m > SEPARATION_HARD_M && s.climb_slot_1_m > 0.0 && s.hard_engaged;
    all &= pass;
    println!("1. separation — collision course");
    println!(
        "   start {:.0} m apart, closing {:.1} m/s, {:.0} s",
        s.start_separation_m,
        s.closing_speed_mps,
        s.ticks as f64 * ados_swarm_control::sim::SIM_DT
    );
    println!(
        "   min separation      {:.3} m   (must exceed {SEPARATION_HARD_M:.1})",
        s.min_separation_m
    );
    println!(
        "   slot 1 climbed      {:.3} m   (lower slot must climb)",
        s.climb_slot_1_m
    );
    println!("   slot 2 climbed      {:.3} m", s.climb_slot_2_m);
    println!(
        "   hard-separation      {} ticks of {}",
        s.hard_ticks, s.ticks
    );
    println!("   -> {}\n", verdict(pass));

    // --- 2 ---------------------------------------------------------------
    let f = scenarios::flocking_to_target();
    let pass = f.arrived == f.fleet && f.min_pairwise_m > SEPARATION_HARD_M;
    all &= pass;
    println!(
        "2. flocking — {} drones, {:.0} m start spread, {:.0} m target",
        f.fleet, f.start_spread_m, f.target_range_m
    );
    println!("   arrived within 30 m {} of {}", f.arrived, f.fleet);
    println!("   worst target error  {:.2} m", f.max_target_error_m);
    println!("   mean target error   {:.2} m", f.mean_target_error_m);
    println!("   start min pairwise  {:.3} m", f.start_min_pairwise_m);
    println!(
        "   min pairwise        {:.3} m   (must exceed {SEPARATION_HARD_M:.1})",
        f.min_pairwise_m
    );
    println!("   -> {}\n", verdict(pass));

    // --- 3 ---------------------------------------------------------------
    let w = scenarios::formation_wedge();
    let pass = w.max_offset_error_m < 3.0 && w.min_pairwise_m > SEPARATION_HARD_M;
    all &= pass;
    println!(
        "3. formation — wedge, {} drones at {:.0} m spacing",
        w.fleet, w.spacing_m
    );
    println!(
        "   worst station error {:.3} m   (must be under 3.0)",
        w.max_offset_error_m
    );
    println!("   mean station error  {:.3} m", w.mean_offset_error_m);
    println!("   start min pairwise  {:.3} m", w.start_min_pairwise_m);
    println!("   min pairwise        {:.3} m", w.min_pairwise_m);
    println!("   -> {}\n", verdict(pass));

    // --- 4 ---------------------------------------------------------------
    let b = scenarios::beacon_loss();
    let pass = b.survivor_neighbours_after == b.survivor_neighbours_before - 1
        && b.isolated_emissions_after_stale == 0
        && b.isolated_drift_m < 0.5
        && b.survivors_still_emitting
        && b.drop_delay_s <= crate_stale_s() + 0.2;
    all &= pass;
    println!("4. beacon loss — one drone's swarm bus killed mid-run");
    println!(
        "   survivor neighbours {} -> {}",
        b.survivor_neighbours_before, b.survivor_neighbours_after
    );
    println!(
        "   dropped after       {:.1} s   (NEIGHBOR_STALE is {:.1} s)",
        b.drop_delay_s,
        crate_stale_s()
    );
    println!(
        "   isolated emissions  {} after the window (must be 0)",
        b.isolated_emissions_after_stale
    );
    println!("   isolated drift      {:.4} m", b.isolated_drift_m);
    println!("   survivors flying    {}", b.survivors_still_emitting);
    println!("   -> {}\n", verdict(pass));

    println!("overall: {}", verdict(all));
    if !all {
        std::process::exit(1);
    }
}

fn crate_stale_s() -> f64 {
    ados_swarm_control::NEIGHBOR_STALE.as_secs_f64()
}
