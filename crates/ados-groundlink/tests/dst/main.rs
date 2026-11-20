//! In-process deterministic simulation (DST) of the ground-station uplink
//! handover between the box and the phones.
//!
//! The box runs its production code: the uplink state and its ops through the
//! command socket's dispatch, a receive generation carrying out every ask on a
//! fake transmitter pair that radiates until both processes exited, the
//! presence watcher's holder timer and frame check, and the LAN route's
//! admission (the phone MAC, the replay guard kept on disk) and signed
//! take-back call. The phones follow the Android app's handover and sign and
//! verify with their own MAC code. Everything runs as tasks on one thread on
//! a paused tokio clock, so 25 s holder windows and 6 s take-back waits pass
//! in microseconds, and every delay, loss, duplicate and replay comes from one
//! seeded generator: a seed names a run exactly, and the meta-tests check
//! that one seed replays byte for byte.
//!
//! Checked every 20 ms tick ([`check`]): the drone never hears two grounds at
//! once except after the box took the uplink back without the holder's
//! confirmation (and then for at most a minute, until the tie rule stops the
//! phone); no ground transmits for longer than the holder window allows; a
//! box in standby with its transmitters asked down never radiates; the box
//! stands by only for a phone whose take it admitted, and falls back exactly
//! after the stale window; no copy of a signed request is admitted twice, and
//! no take-back call is accepted twice or from the wrong context; every
//! genuine take-back call verifies at the phone and its answer is waited for.
//! After a calm tail exactly one ground holds the uplink and the box agrees
//! which. Latency stays with the real-time benches.
//!
//! Every test name starts `dst_`: `cargo test -p ados-groundlink --test dst`.

#![cfg(unix)]

mod boxside;
mod check;
mod net;
mod phone;
mod rng;
mod scenarios;
mod world;

use std::time::Instant;

use scenarios::ALL;

/// Seeds flown per scenario in the matrix.
const SEEDS: std::ops::RangeInclusive<u64> = 1..=4;

fn scenario(name: &str) -> scenarios::Scenario {
    ALL.iter()
        .find(|(n, _)| *n == name)
        .map(|(_, s)| *s)
        .expect("a scenario of that name")
}

/// Run `name` on every matrix seed; print simulated against wall time.
fn matrix(name: &'static str) -> Vec<world::Run> {
    let plan = scenario(name);
    let wall = Instant::now();
    let runs: Vec<_> = SEEDS.map(|seed| world::run(name, seed, plan)).collect();
    let sim_s: f64 = runs.iter().map(|r| r.sim_ms as f64 / 1000.0).sum();
    let wall_s = wall.elapsed().as_secs_f64();
    eprintln!(
        "dst {name}: {} seeds, {sim_s:.0} sim s in {wall_s:.2} wall s ({:.0}x real time)",
        SEEDS.count(),
        sim_s / wall_s
    );
    for (seed, r) in SEEDS.zip(&runs) {
        eprintln!("  seed {seed}: {:?}", r.ledger.stats);
    }
    runs
}

#[test]
fn dst_handover() {
    for r in matrix("handover") {
        assert!(r.ledger.stats.takes >= 1, "{:?}", r.ledger.stats);
    }
}

#[test]
fn dst_take_back() {
    for r in matrix("take_back") {
        let s = &r.ledger.stats;
        assert!(
            s.take_backs_confirmed + s.take_backs_unconfirmed >= 1,
            "{s:?}"
        );
    }
}

#[test]
fn dst_silent_holder() {
    for r in matrix("silent_holder") {
        assert!(r.ledger.stats.fallbacks >= 1, "{:?}", r.ledger.stats);
    }
}

#[test]
fn dst_tie_three_grounds() {
    matrix("tie_three_grounds");
}

#[test]
fn dst_replay_restart() {
    // Every admitted replay already failed the run; here, the eavesdropper
    // reached both the route and the phones on every seed.
    for r in matrix("replay_restart") {
        let s = &r.ledger.stats;
        assert!(s.replays_refused > 0 && s.yields_refused > 0, "{s:?}");
    }
}

#[test]
fn dst_take_back_race() {
    for r in matrix("take_back_race") {
        assert!(
            r.ledger.stats.take_backs_confirmed >= 1,
            "{:?}",
            r.ledger.stats
        );
    }
}

/// One seed replays byte for byte: every delivery, reply and transition.
#[test]
fn dst_same_seed_replays_byte_identical() {
    for (name, plan) in ALL {
        let a = world::run(name, 7, *plan);
        let b = world::run(name, 7, *plan);
        if let Some((line, x, y)) = a.trace.first_difference(&b.trace) {
            panic!("{name}: seed 7 diverged at trace line {line}:\n{x}\n{y}");
        }
        assert_eq!(a.trace.hash(), b.trace.hash(), "{name}");
        assert!(a.trace.bytes.len() > 10_000, "{name}: a trace was written");
    }
}

/// Different seeds run different handovers.
#[test]
fn dst_different_seeds_differ() {
    for (name, plan) in ALL {
        let a = world::run(name, 11, *plan).trace.hash();
        let b = world::run(name, 12, *plan).trace.hash();
        assert_ne!(a, b, "{name}: seeds 11 and 12 gave one trace");
    }
}
