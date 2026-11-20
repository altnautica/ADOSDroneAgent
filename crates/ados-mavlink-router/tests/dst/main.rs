//! In-process deterministic simulation (DST) of the MAVLink router.
//!
//! The router's FC link, its request ticks, the mission mirror and the pilot
//! link run as they do in the service, but on one thread against simulated
//! peers: an ArduPilot-like flight controller ([`fc`]), a ground station on
//! the proxy ([`gcs`]) and the pilot daemon with its aux radio downlink
//! ([`downlink`]). Every link is a seeded lossy, delaying, duplicating and
//! reordering byte channel ([`chan`]); time is simulated, 10 ms a tick, both
//! the router's ADOS time (its sim-clock feed) and its steady time (a
//! paused Tokio clock). No socket, no real timer: a seed names a run, and
//! the meta-tests check one seed replays byte for byte.
//!
//! What it proves is logic and coverage under faults: link liveness and
//! re-acquisition, stream re-requests, the mirror's download deadlines and
//! retries, the pilot downlink's accounting, the command gates. It says
//! nothing about latency or throughput.
//!
//! Every test name starts `dst_`: `cargo test -p ados-mavlink-router --test
//! dst` runs them; `DST_SEED=<n>` flies one seed; `DST_TRACE_DIR=<dir>`
//! writes every run's trace.

mod chan;
mod checks;
mod downlink;
mod fc;
mod gcs;
mod mav;
mod pilot;
mod rng;
mod router;
mod scenarios;
mod sim;

use std::time::Instant;

use scenarios::ALL;

/// Seeds flown per scenario in the matrix.
const SEEDS: std::ops::RangeInclusive<u64> = 1..=4;

/// Fly `name` on every matrix seed (or `DST_SEED` alone); print simulated
/// against wall time.
fn matrix(name: &str) {
    let (_, fly) = ALL
        .iter()
        .find(|(n, _)| *n == name)
        .expect("a scenario of that name");
    let seeds: Vec<u64> = match std::env::var("DST_SEED").ok().and_then(|s| s.parse().ok()) {
        Some(s) => vec![s],
        None => SEEDS.collect(),
    };
    let wall = Instant::now();
    let mut sim_ms = 0;
    for &seed in &seeds {
        sim_ms += fly(seed).now;
    }
    let wall_s = wall.elapsed().as_secs_f64();
    let sim_s = sim_ms as f64 / 1000.0;
    eprintln!(
        "dst {name}: {} seeds, {sim_s:.0} sim s in {wall_s:.2} wall s ({:.0}x real time)",
        seeds.len(),
        sim_s / wall_s
    );
}

#[test]
fn dst_fc_link() {
    matrix("fc_link");
}

#[test]
fn dst_mission_mirror() {
    matrix("mission_mirror");
}

#[test]
fn dst_pilot_downlink() {
    matrix("pilot_downlink");
}

#[test]
fn dst_gate_crsf() {
    matrix("gate_crsf");
}

#[test]
fn dst_gate_visor() {
    matrix("gate_visor");
}

/// One seed replays byte for byte: every event, every link counter and the
/// router's state each 100 ms.
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
