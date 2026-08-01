//! A deterministic point-mass swarm simulator for the four flight-gate scenarios.
//!
//! # Why this exists
//!
//! First flight is gated on software-in-the-loop, with a real autopilot per
//! aircraft. Standing that up is a separate piece of tooling, so the scenarios
//! are additionally reproduced here at the control-law level: same laws, same
//! 10 Hz cadence, same 2 Hz beacon rate with the same wire quantisation and the
//! same dead reckoning, against a first-order velocity plant instead of a real
//! autopilot. That is enough to falsify a control-law bug, which is what the four
//! scenarios are for; it is NOT a substitute for SITL, and it makes no claim
//! about the autopilot's own attitude loop, its EKF, or its failsafes.
//!
//! Deterministic on purpose: no wall clock, no RNG unless seeded here, so a
//! scenario that passes in CI passes identically on a laptop.

use std::collections::BTreeMap;
use std::time::Instant;

use crate::config::SwarmControlConfig;
use crate::controller::{NeighborFix, OwnState, SwarmController, CONTROL_PERIOD};
use crate::geo::{GeoOrigin, Ned};
use crate::neighbor::{STATUS_ARMED, STATUS_EMERGENCY, STATUS_GPS_OK, STATUS_GUIDED};
use crate::precedence::ModePrecedence;
use crate::separation::min_pairwise_distance;

/// Simulation step, seconds — the control period, so one step is one tick.
pub const SIM_DT: f64 = 0.1;

/// First-order velocity-tracking time constant, seconds. A plausible small
/// multirotor velocity loop; the point is that the plant LAGS the command, which
/// is what turns a marginal safety margin into a breach.
pub const VELOCITY_TAU_S: f64 = 0.3;

/// Control ticks per beacon. 2 Hz beacons against a 10 Hz loop.
pub const BEACON_PERIOD_TICKS: usize = 5;

/// A beacon on the wire, quantised exactly as the 20-byte frame is.
#[derive(Debug, Clone, Copy)]
struct SimBeacon {
    at_tick: usize,
    lat_e7: i32,
    lon_e7: i32,
    alt_dm: i16,
    vx_cms: i16,
    vy_cms: i16,
    vz_cms: i16,
    status: u8,
}

/// One simulated aircraft.
pub struct SimDrone {
    pub slot: u8,
    pub pos: Ned,
    pub vel: Ned,
    pub armed: bool,
    pub guided: bool,
    /// Killing this stops the drone both transmitting and receiving beacons — the
    /// beacon-loss scenario.
    pub bus_alive: bool,
    pub controller: SwarmController,
    /// Altitude at spawn, so a scenario can assert a climb.
    pub spawn_alt_m: f64,
    /// Whether the last tick emitted a setpoint. The controller's own counters
    /// aggregate; this is the per-tick view a scenario asserts on.
    pub emitted_last: bool,
}

impl SimDrone {
    pub fn new(slot: u8, pos: Ned, cfg: &SwarmControlConfig, slots: &[u8]) -> Self {
        Self {
            slot,
            pos,
            vel: Ned::ZERO,
            armed: true,
            guided: true,
            bus_alive: true,
            controller: SwarmController::new(cfg, slots),
            spawn_alt_m: -pos.d,
            emitted_last: false,
        }
    }

    pub fn with_velocity(mut self, vel: Ned) -> Self {
        self.vel = vel;
        self
    }

    /// Altitude relative to the world origin, metres (up-positive).
    pub fn alt_m(&self) -> f64 {
        -self.pos.d
    }

    pub fn climbed_m(&self) -> f64 {
        self.alt_m() - self.spawn_alt_m
    }
}

/// One sampled instant.
#[derive(Debug, Clone, PartialEq)]
pub struct SimSample {
    pub t_s: f64,
    /// Smallest pairwise 3-D separation across the whole fleet, metres.
    pub min_pairwise_m: Option<f64>,
    pub positions: Vec<(u8, Ned)>,
    pub precedence: Vec<(u8, ModePrecedence)>,
    /// Whether each drone emitted a setpoint on this tick.
    pub emitted: Vec<(u8, bool)>,
    pub neighbors_seen: Vec<(u8, usize)>,
}

impl SimSample {
    pub fn precedence_of(&self, slot: u8) -> Option<ModePrecedence> {
        self.precedence
            .iter()
            .find(|(s, _)| *s == slot)
            .map(|(_, p)| *p)
    }

    pub fn emitted_by(&self, slot: u8) -> bool {
        self.emitted
            .iter()
            .find(|(s, _)| *s == slot)
            .is_some_and(|(_, e)| *e)
    }

    pub fn neighbors_of(&self, slot: u8) -> usize {
        self.neighbors_seen
            .iter()
            .find(|(s, _)| *s == slot)
            .map_or(0, |(_, n)| *n)
    }
}

/// The whole run.
#[derive(Debug, Clone)]
pub struct SimReport {
    pub ticks: usize,
    /// The worst separation seen at ANY sampled instant, metres.
    pub min_pairwise_m: f64,
    pub samples: Vec<SimSample>,
}

impl SimReport {
    pub fn last(&self) -> &SimSample {
        self.samples.last().expect("a run has at least one sample")
    }
}

/// The simulated world. Broadcast medium, lossless, so the only reason a drone
/// misses a beacon is that the sender stopped sending.
pub struct SimWorld {
    pub origin: GeoOrigin,
    pub drones: Vec<SimDrone>,
    beacons: BTreeMap<u8, SimBeacon>,
    t0: Instant,
    tick: usize,
}

impl SimWorld {
    pub fn new(origin: GeoOrigin, drones: Vec<SimDrone>) -> Self {
        Self {
            origin,
            drones,
            beacons: BTreeMap::new(),
            t0: Instant::now(),
            tick: 0,
        }
    }

    pub fn drone(&self, slot: u8) -> Option<&SimDrone> {
        self.drones.iter().find(|d| d.slot == slot)
    }

    pub fn drone_mut(&mut self, slot: u8) -> Option<&mut SimDrone> {
        self.drones.iter_mut().find(|d| d.slot == slot)
    }

    pub fn t_s(&self) -> f64 {
        self.tick as f64 * SIM_DT
    }

    /// The world clock, in the same `Instant` domain the controllers see. A
    /// scenario that re-issues an operator directive each tick must use this, not
    /// `Instant::now()`, or the directive's TTL is measured against a clock the
    /// simulation is not running on.
    pub fn now(&self) -> Instant {
        self.t0 + CONTROL_PERIOD * self.tick as u32
    }

    /// Advance one control period and return the sample taken BEFORE the plant
    /// moved, so a sample's `min_pairwise_m` is a real instant of the trajectory.
    pub fn step(&mut self) -> SimSample {
        let now = self.t0 + CONTROL_PERIOD * self.tick as u32;

        if self.tick.is_multiple_of(BEACON_PERIOD_TICKS) {
            for d in &self.drones {
                if !d.bus_alive {
                    continue;
                }
                self.beacons.insert(d.slot, self.beacon_of(d));
            }
        }

        // Gather every controller input under immutable borrows first; the
        // mutable tick + plant integration then runs over indices, so no drone
        // observes another drone's post-move state within the same tick.
        let mut inputs: Vec<(OwnState, Vec<NeighborFix>)> = Vec::with_capacity(self.drones.len());
        for d in &self.drones {
            let (lat, lon, _) = self.origin.to_geo(d.pos);
            inputs.push((
                OwnState {
                    slot: d.slot,
                    lat_deg: lat,
                    lon_deg: lon,
                    alt_rel_m: d.alt_m(),
                    vn: d.vel.n,
                    ve: d.vel.e,
                    vd: d.vel.d,
                    armed: d.armed,
                    guided: d.guided,
                    // The simulated plant refreshes every drone's position each
                    // tick by construction, so the fix is always current here.
                    fix_age: Some(std::time::Duration::ZERO),
                },
                if d.bus_alive {
                    self.fixes_for(d.slot)
                } else {
                    Vec::new()
                },
            ));
        }

        let mut sample = SimSample {
            t_s: self.t_s(),
            // Sampled BEFORE the plant moves, so a sample is a real instant of
            // the trajectory rather than the state after integration.
            min_pairwise_m: min_pairwise_distance(
                &self.drones.iter().map(|d| d.pos).collect::<Vec<_>>(),
            ),
            positions: self.drones.iter().map(|d| (d.slot, d.pos)).collect(),
            precedence: Vec::with_capacity(self.drones.len()),
            emitted: Vec::with_capacity(self.drones.len()),
            neighbors_seen: Vec::with_capacity(self.drones.len()),
        };

        let alpha = (SIM_DT / VELOCITY_TAU_S).min(1.0);
        for (i, (own, fixes)) in inputs.iter().enumerate() {
            let outcome = self.drones[i].controller.tick(own, fixes, now);
            let want = outcome
                .setpoint
                .map(|s| Ned::new(s.vn as f64, s.ve as f64, s.vd as f64))
                // No setpoint means the FC holds: the commanded velocity bleeds
                // off and nothing new is asked for.
                .unwrap_or(Ned::ZERO);
            let d = &mut self.drones[i];
            d.emitted_last = outcome.setpoint.is_some();
            d.vel = d.vel + (want - d.vel).scale(alpha);
            d.pos = d.pos + d.vel.scale(SIM_DT);
            sample.precedence.push((d.slot, outcome.precedence));
            sample.emitted.push((d.slot, outcome.setpoint.is_some()));
            sample.neighbors_seen.push((d.slot, fixes.len()));
        }
        self.tick += 1;
        sample
    }

    /// Run `ticks` control periods, sampling every one.
    pub fn run(&mut self, ticks: usize) -> SimReport {
        let mut samples = Vec::with_capacity(ticks);
        let mut min = f64::INFINITY;
        for _ in 0..ticks {
            let s = self.step();
            if let Some(m) = s.min_pairwise_m {
                min = min.min(m);
            }
            samples.push(s);
        }
        SimReport {
            ticks,
            min_pairwise_m: min,
            samples,
        }
    }

    fn beacon_of(&self, d: &SimDrone) -> SimBeacon {
        let (lat, lon, alt) = self.origin.to_geo(d.pos);
        let mut status = STATUS_GPS_OK;
        if d.armed {
            status |= STATUS_ARMED;
        }
        if d.guided {
            status |= STATUS_GUIDED;
        }
        if d.controller.emergency() {
            status |= STATUS_EMERGENCY;
        }
        status |= d.controller.precedence().as_status_bits();
        SimBeacon {
            at_tick: self.tick,
            lat_e7: crate::geo::deg_to_e7(lat),
            lon_e7: crate::geo::deg_to_e7(lon),
            alt_dm: (alt * 10.0).round().clamp(i16::MIN as f64, i16::MAX as f64) as i16,
            vx_cms: (d.vel.n * 100.0)
                .round()
                .clamp(i16::MIN as f64, i16::MAX as f64) as i16,
            vy_cms: (d.vel.e * 100.0)
                .round()
                .clamp(i16::MIN as f64, i16::MAX as f64) as i16,
            vz_cms: (d.vel.d * 100.0)
                .round()
                .clamp(i16::MIN as f64, i16::MAX as f64) as i16,
            status,
        }
    }

    /// Every heard neighbour, dead-reckoned forward to now, with anything older
    /// than the staleness window dropped — exactly what `NeighborTable` serves.
    fn fixes_for(&self, slot: u8) -> Vec<NeighborFix> {
        let stale_ticks = (crate::NEIGHBOR_STALE.as_secs_f64() / SIM_DT).round() as usize;
        self.beacons
            .iter()
            .filter(|(s, _)| **s != slot)
            .filter_map(|(s, b)| {
                let age_ticks = self.tick.saturating_sub(b.at_tick);
                if age_ticks >= stale_ticks {
                    return None;
                }
                let dt = age_ticks as f64 * SIM_DT;
                let (vn, ve, vd) = (
                    b.vx_cms as f64 / 100.0,
                    b.vy_cms as f64 / 100.0,
                    b.vz_cms as f64 / 100.0,
                );
                let lat = crate::geo::e7_to_deg(b.lat_e7);
                let lon = crate::geo::e7_to_deg(b.lon_e7);
                let alt = b.alt_dm as f64 / 10.0;
                // Dead reckon in the local frame, then hand back geodetic, which
                // is the same order `NeighborTable::predicted` uses.
                let o = GeoOrigin::new(lat, lon, alt);
                let (plat, plon, palt) = o.to_geo(Ned::new(vn * dt, ve * dt, vd * dt));
                Some(NeighborFix {
                    slot: *s,
                    lat_deg: plat,
                    lon_deg: plon,
                    alt_m: palt,
                    vn,
                    ve,
                    vd,
                    status: b.status,
                })
            })
            .collect()
    }
}

/// A tiny deterministic PRNG so a "random start" is reproducible. xorshift64*.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `[-1, 1]`.
    pub fn signed_unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plant_tracks_a_commanded_velocity() {
        let cfg = SwarmControlConfig {
            enabled: true,
            mode: "hold".into(),
            ..Default::default()
        };
        let mut w = SimWorld::new(
            GeoOrigin::new(12.9716, 77.5946, 0.0),
            vec![SimDrone::new(1, Ned::new(0.0, 0.0, -30.0), &cfg, &[1])],
        );
        // A lone drone in hold with no neighbours goes stale and stops commanding,
        // so it must not drift.
        let r = w.run(60);
        assert!((r.last().positions[0].1 - Ned::new(0.0, 0.0, -30.0)).norm() < 1e-9);
    }

    #[test]
    fn beacons_are_quantised_and_dead_reckoned_between_them() {
        let cfg = SwarmControlConfig {
            enabled: true,
            ..Default::default()
        };
        let mut w = SimWorld::new(
            GeoOrigin::new(12.9716, 77.5946, 0.0),
            vec![
                SimDrone::new(1, Ned::new(0.0, 0.0, -30.0), &cfg, &[1, 2]),
                SimDrone::new(2, Ned::new(50.0, 0.0, -30.0), &cfg, &[1, 2])
                    .with_velocity(Ned::new(5.0, 0.0, 0.0)),
            ],
        );
        // Publish, then read at zero age: this is the pure quantisation path.
        // The wire carries decimetres of altitude and centimetres per second, so
        // the round trip is tight but not exact.
        w.beacons.insert(2, w.beacon_of(&w.drones[1]));
        let fixes = w.fixes_for(1);
        assert_eq!(fixes.len(), 1);
        let seen = w
            .origin
            .to_ned(fixes[0].lat_deg, fixes[0].lon_deg, fixes[0].alt_m);
        assert!(
            (seen - Ned::new(50.0, 0.0, -30.0)).norm() < 0.06,
            "{seen:?}"
        );
        assert!(
            (fixes[0].vn - 5.0).abs() < 0.01,
            "velocity survives: {fixes:?}"
        );

        // One tick later, with no fresh beacon, the fix must have been dead
        // reckoned FORWARD by one period at the reported velocity — that is the
        // whole reason a 2 Hz beacon can drive a 10 Hz loop.
        w.tick += 1;
        let later = w.fixes_for(1);
        let advanced = w
            .origin
            .to_ned(later[0].lat_deg, later[0].lon_deg, later[0].alt_m);
        assert!(
            (advanced.n - seen.n - 5.0 * SIM_DT).abs() < 0.01,
            "expected {} m of dead reckoning, got {}",
            5.0 * SIM_DT,
            advanced.n - seen.n
        );
    }

    #[test]
    fn a_dead_bus_stops_both_directions() {
        let cfg = SwarmControlConfig {
            enabled: true,
            ..Default::default()
        };
        let mut w = SimWorld::new(
            GeoOrigin::new(12.9716, 77.5946, 0.0),
            vec![
                SimDrone::new(1, Ned::new(0.0, 0.0, -30.0), &cfg, &[1, 2]),
                SimDrone::new(2, Ned::new(50.0, 0.0, -30.0), &cfg, &[1, 2]),
            ],
        );
        w.step();
        assert_eq!(w.fixes_for(1).len(), 1);
        w.drone_mut(2).expect("slot 2").bus_alive = false;
        let s = w.step();
        assert_eq!(s.neighbors_of(2), 0, "a dead bus hears nothing");
    }

    #[test]
    fn the_rng_is_deterministic_and_bounded() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..1000 {
            let x = a.signed_unit();
            assert_eq!(x, b.signed_unit());
            assert!((-1.0..=1.0).contains(&x), "{x}");
        }
        assert_ne!(Rng::new(1).next_u64(), Rng::new(2).next_u64());
    }
}
