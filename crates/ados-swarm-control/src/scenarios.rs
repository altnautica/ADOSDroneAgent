//! The four flight-gate scenarios, as reusable runs.
//!
//! First flight is gated on software-in-the-loop, with a real autopilot per
//! aircraft. These four are the control-law level equivalents that run without
//! one: identical laws, identical 10 Hz cadence, identical 2 Hz beacons with
//! wire quantisation and dead reckoning, against the first-order plant in
//! [`crate::sim`]. They falsify a control-law bug; they say nothing about the
//! autopilot's attitude loop, its EKF or its failsafes, and they are not a
//! substitute for SITL before flight.
//!
//! Each function returns measured numbers rather than a boolean, so the same run
//! backs both the assertions in `tests/scenarios.rs` and the printed report in
//! `examples/swarm_scenarios.rs`.

use crate::config::{SeparationConfig, SwarmControlConfig};
use crate::controller::OperatorDirective;
use crate::formation::{Formation, FormationAnchor, FormationName};
use crate::geo::{GeoOrigin, Ned};
use crate::precedence::ModePrecedence;
use crate::separation::min_pairwise_distance;
use crate::sim::{Rng, SimDrone, SimWorld, SIM_DT};

/// Bench origin. Any origin works; a real one keeps the projection honest.
fn origin() -> GeoOrigin {
    GeoOrigin::new(12.9716, 77.5946, 0.0)
}

fn cfg(mode: &str) -> SwarmControlConfig {
    SwarmControlConfig {
        enabled: true,
        mode: mode.into(),
        ..Default::default()
    }
}

/// `n` random positions inside a box of half-extent `half`, no two closer than
/// `min_gap`, at `-alt` down with `alt_spread` of vertical scatter.
///
/// Rejection sampled on purpose. An unconstrained "random start within 100 m"
/// routinely places two of eight drones three metres apart, and a run that BEGINS
/// inside the safety floor measures the initial condition rather than the
/// controller — no velocity command can un-violate a violation that was there at
/// t = 0. Real fleets launch from separated pads, which is what this models.
/// Falls back to an evenly spaced ring if the box is too tight to sample, so the
/// helper can never loop forever or return an illegal set.
fn scatter(
    rng: &mut Rng,
    n: usize,
    half: f64,
    alt: f64,
    alt_spread: f64,
    min_gap: f64,
) -> Vec<Ned> {
    let mut out: Vec<Ned> = Vec::with_capacity(n);
    'next: for i in 0..n {
        for _ in 0..20_000 {
            let p = Ned::new(
                rng.signed_unit() * half,
                rng.signed_unit() * half,
                -alt + rng.signed_unit() * alt_spread,
            );
            if out.iter().all(|q| (*q - p).norm() >= min_gap) {
                out.push(p);
                continue 'next;
            }
        }
        // Too dense to sample: place the remainder on a ring wide enough that
        // adjacent stations are `min_gap` apart.
        let r = (min_gap / (2.0 * (std::f64::consts::PI / n as f64).sin())).max(half);
        let theta = std::f64::consts::TAU * i as f64 / n as f64;
        out.push(Ned::new(r * theta.cos(), r * theta.sin(), -alt));
    }
    out
}

// ------------------------------------------------------------- 1. separation

/// Scenario 1 result.
#[derive(Debug, Clone, PartialEq)]
pub struct SeparationScenario {
    pub ticks: usize,
    /// Worst separation over the whole run, metres.
    pub min_separation_m: f64,
    /// Metres climbed by the lower-slot drone.
    pub climb_slot_1_m: f64,
    pub climb_slot_2_m: f64,
    /// Whether the hard override ever took the vehicle.
    pub hard_engaged: bool,
    /// Ticks in which at least one drone read `hard-separation`.
    pub hard_ticks: usize,
    pub start_separation_m: f64,
    pub closing_speed_mps: f64,
}

/// Two drones on a collision course 20 m apart, closing at 4 m/s.
///
/// The closure is sustained by re-issuing an operator directive 2 m ahead of each
/// drone along the line to the other every tick — a fixed distant waypoint would
/// saturate at the speed ceiling and a fixed near one would be reached and stop,
/// and neither is a 4 m/s collision course.
pub fn separation_collision_course() -> SeparationScenario {
    const START_GAP: f64 = 20.0;
    const PER_DRONE_MPS: f64 = 2.0; // 4 m/s of closure between the pair
    let ticks = 900; // 90 s
    let o = origin();
    let c = cfg("hold");
    let mut w = SimWorld::new(
        o,
        vec![
            SimDrone::new(1, Ned::new(0.0, 0.0, -30.0), &c, &[1, 2]),
            SimDrone::new(2, Ned::new(START_GAP, 0.0, -30.0), &c, &[1, 2]),
        ],
    );

    let mut min = f64::INFINITY;
    let mut hard_ticks = 0;
    for _ in 0..ticks {
        let now = w.now();
        let p1 = w.drone(1).expect("slot 1").pos;
        let p2 = w.drone(2).expect("slot 2").pos;
        for (slot, from, to) in [(1u8, p1, p2), (2u8, p2, p1)] {
            let aim = from + (to - from).unit().scale(PER_DRONE_MPS);
            let (lat, lon, alt) = o.to_geo(aim);
            w.drone_mut(slot)
                .expect("slot")
                .controller
                .set_operator_directive(
                    OperatorDirective::Goto {
                        lat_deg: lat,
                        lon_deg: lon,
                        alt_m: alt,
                    },
                    now,
                );
        }
        let s = w.step();
        if let Some(m) = s.min_pairwise_m {
            min = min.min(m);
        }
        if s.precedence
            .iter()
            .any(|(_, p)| *p == ModePrecedence::HardSeparation)
        {
            hard_ticks += 1;
        }
    }

    SeparationScenario {
        ticks,
        min_separation_m: min,
        climb_slot_1_m: w.drone(1).expect("slot 1").climbed_m(),
        climb_slot_2_m: w.drone(2).expect("slot 2").climbed_m(),
        hard_engaged: hard_ticks > 0,
        hard_ticks,
        start_separation_m: START_GAP,
        closing_speed_mps: PER_DRONE_MPS * 2.0,
    }
}

// --------------------------------------------------------------- 2. flocking

/// Scenario 2 result.
#[derive(Debug, Clone, PartialEq)]
pub struct FlockingScenario {
    pub ticks: usize,
    pub fleet: usize,
    /// Worst separation over the whole run, metres.
    pub min_pairwise_m: f64,
    /// Worst final distance to the target, metres.
    pub max_target_error_m: f64,
    pub mean_target_error_m: f64,
    /// How many drones ended within 30 m of the target.
    pub arrived: usize,
    pub target_range_m: f64,
    pub start_spread_m: f64,
    /// Closest pair at t = 0, so the run's floor can be read against a legal
    /// starting geometry rather than a random violation.
    pub start_min_pairwise_m: f64,
}

/// Eight drones from a random 100 m start, flocking to a target 500 m away.
pub fn flocking_to_target() -> FlockingScenario {
    const FLEET: usize = 8;
    const SPREAD: f64 = 100.0;
    const RANGE: f64 = 500.0;
    const ARRIVE_M: f64 = 30.0;
    let ticks = 1600; // 160 s, generous for a 500 m transit at the speed ceiling
    let o = origin();
    let c = cfg("flocking");
    let slots: Vec<u8> = (1..=FLEET as u8).collect();
    let target = Ned::new(RANGE, 0.0, -30.0);
    let (tlat, tlon, talt) = o.to_geo(target);

    // Seeded so a "random start" is reproducible run to run, and rejection
    // sampled so it starts outside the soft separation radius.
    let mut rng = Rng::new(0x5EED_5A1D);
    let starts = scatter(
        &mut rng,
        FLEET,
        SPREAD * 0.5,
        30.0,
        5.0,
        crate::separation::SEPARATION_RADIUS_M,
    );
    let start_min = min_pairwise_distance(&starts).unwrap_or(f64::INFINITY);
    let mut drones = Vec::with_capacity(FLEET);
    for (slot, pos) in slots.iter().zip(&starts) {
        let mut d = SimDrone::new(*slot, *pos, &c, &slots);
        d.controller.set_target(Some((tlat, tlon, talt)));
        drones.push(d);
    }
    let mut w = SimWorld::new(o, drones);
    let report = w.run(ticks);

    let errors: Vec<f64> = w.drones.iter().map(|d| (d.pos - target).norm()).collect();
    FlockingScenario {
        ticks,
        fleet: FLEET,
        min_pairwise_m: report.min_pairwise_m,
        max_target_error_m: errors.iter().copied().fold(0.0, f64::max),
        mean_target_error_m: errors.iter().sum::<f64>() / FLEET as f64,
        arrived: errors.iter().filter(|e| **e <= ARRIVE_M).count(),
        target_range_m: RANGE,
        start_spread_m: SPREAD,
        start_min_pairwise_m: start_min,
    }
}

// -------------------------------------------------------------- 3. formation

/// Scenario 3 result.
#[derive(Debug, Clone, PartialEq)]
pub struct FormationScenario {
    pub ticks: usize,
    pub fleet: usize,
    pub spacing_m: f64,
    /// Worst steady-state station error, metres.
    pub max_offset_error_m: f64,
    pub mean_offset_error_m: f64,
    /// Worst separation over the whole run, metres.
    pub min_pairwise_m: f64,
    /// Closest pair at t = 0.
    pub start_min_pairwise_m: f64,
}

/// Seven drones commanded into a wedge from a scattered start.
pub fn formation_wedge() -> FormationScenario {
    const FLEET: usize = 7;
    const SPACING: f64 = 12.0;
    let ticks = 1200; // 120 s
    let o = origin();
    let mut c = cfg("formation");
    c.default_formation = "wedge".into();
    c.default_spacing = SPACING as i64;
    let slots: Vec<u8> = (1..=FLEET as u8).collect();

    let mut rng = Rng::new(0xC0FF_EE01);
    let starts = scatter(
        &mut rng,
        FLEET,
        40.0,
        30.0,
        3.0,
        crate::separation::SEPARATION_RADIUS_M,
    );
    let start_min = min_pairwise_distance(&starts).unwrap_or(f64::INFINITY);
    let drones: Vec<SimDrone> = slots
        .iter()
        .zip(&starts)
        .map(|(slot, pos)| SimDrone::new(*slot, *pos, &c, &slots))
        .collect();
    let mut w = SimWorld::new(o, drones);
    let report = w.run(ticks);

    // Steady-state error against the anchor the fleet actually settled on.
    let table = Formation::built_in(
        FormationName::Wedge,
        &slots,
        SPACING,
        FormationAnchor::Centroid,
    );
    let mut centroid = Ned::ZERO;
    for d in &w.drones {
        centroid = centroid + d.pos;
    }
    centroid = centroid.scale(1.0 / FLEET as f64);
    let errors: Vec<f64> = w
        .drones
        .iter()
        .map(|d| {
            let station = table.station(d.slot).expect("registered slot");
            (d.pos - (centroid + station)).norm()
        })
        .collect();

    FormationScenario {
        ticks,
        fleet: FLEET,
        spacing_m: SPACING,
        max_offset_error_m: errors.iter().copied().fold(0.0, f64::max),
        mean_offset_error_m: errors.iter().sum::<f64>() / FLEET as f64,
        min_pairwise_m: report.min_pairwise_m,
        start_min_pairwise_m: start_min,
    }
}

// ------------------------------------------------------------ 4. beacon loss

/// Scenario 4 result.
#[derive(Debug, Clone, PartialEq)]
pub struct BeaconLossScenario {
    pub ticks: usize,
    pub kill_at_tick: usize,
    /// Neighbours the surviving drones could still see, once the window elapsed.
    pub survivor_neighbours_after: usize,
    /// Neighbours the survivors saw before the loss.
    pub survivor_neighbours_before: usize,
    /// Ticks after the staleness window in which the isolated drone still emitted.
    pub isolated_emissions_after_stale: usize,
    /// How far the isolated drone travelled after the window elapsed, metres.
    pub isolated_drift_m: f64,
    /// Whether the survivors kept flying.
    pub survivors_still_emitting: bool,
    /// How long after the kill the survivors' tables actually dropped it.
    pub drop_delay_s: f64,
}

/// Three drones in formation; one drone's swarm bus is killed mid-run.
pub fn beacon_loss() -> BeaconLossScenario {
    const FLEET: usize = 3;
    let ticks = 400; // 40 s
    let kill_at = 100; // 10 s in
    let o = origin();
    let mut c = cfg("formation");
    c.default_spacing = 15;
    // Wider soft radius has no bearing here; keep the shipped safety pair.
    c.separation = SeparationConfig::default();
    let slots: Vec<u8> = (1..=FLEET as u8).collect();
    let mut w = SimWorld::new(
        o,
        vec![
            SimDrone::new(1, Ned::new(0.0, -15.0, -30.0), &c, &slots),
            SimDrone::new(2, Ned::new(0.0, 0.0, -30.0), &c, &slots),
            SimDrone::new(3, Ned::new(0.0, 15.0, -30.0), &c, &slots),
        ],
    );

    let stale_ticks = (crate::NEIGHBOR_STALE.as_secs_f64() / SIM_DT).round() as usize;
    let mut before = 0usize;
    let mut after = 0usize;
    let mut emissions_after = 0usize;
    let mut survivors_emitting = true;
    let mut drop_tick = None;
    let mut pos_at_settle = None;

    for tick in 0..ticks {
        if tick == kill_at {
            w.drone_mut(3).expect("slot 3").bus_alive = false;
        }
        let s = w.step();
        if tick == kill_at - 1 {
            before = s.neighbors_of(1);
        }
        if tick > kill_at && drop_tick.is_none() && s.neighbors_of(1) < before {
            drop_tick = Some(tick);
        }
        // Two full staleness windows past the kill: the isolated drone must be
        // silent and the survivors must have moved on.
        if tick >= kill_at + stale_ticks + 5 {
            if pos_at_settle.is_none() {
                pos_at_settle = Some(w.drone(3).expect("slot 3").pos);
            }
            after = s.neighbors_of(1);
            if s.emitted_by(3) {
                emissions_after += 1;
            }
            if !s.emitted_by(1) && !s.emitted_by(2) {
                survivors_emitting = false;
            }
        }
    }

    BeaconLossScenario {
        ticks,
        kill_at_tick: kill_at,
        survivor_neighbours_after: after,
        survivor_neighbours_before: before,
        isolated_emissions_after_stale: emissions_after,
        isolated_drift_m: pos_at_settle
            .map(|p| (w.drone(3).expect("slot 3").pos - p).norm())
            .unwrap_or(f64::NAN),
        survivors_still_emitting: survivors_emitting,
        drop_delay_s: drop_tick
            .map(|t| (t - kill_at) as f64 * SIM_DT)
            .unwrap_or(f64::NAN),
    }
}

/// Smallest pairwise distance among a set of drone positions — re-exported for a
/// harness that wants to sample it itself.
pub fn min_separation(positions: &[Ned]) -> Option<f64> {
    min_pairwise_distance(positions)
}
