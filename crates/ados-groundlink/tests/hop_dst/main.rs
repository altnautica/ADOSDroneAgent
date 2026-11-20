//! In-process deterministic simulation (DST) of the coordinated channel hop:
//! the radio's frequency-hop supervisor and process respawn on the drone, the
//! hop follower and valid-packet watchdog on one or two ground stations.
//!
//! The drone runs the real hop supervisor ([`ados_radio::daemon::hop_supervisor::run_hop_loop`]),
//! its presence beacon emitter, its `HopState` and the respawn-channel rule
//! over a simulated radio ([`drone::SimRadio`]), inside a model of the radio
//! service's respawn loop. Each ground station ([`ground`]) runs this crate's
//! real control-frame dispatch, hop follower, presence cache and valid-packet
//! watchdog with its channel acquirer over a simulated receive radio, inside a
//! model of the receive loop's generations. It lives here, not in `ados-radio`,
//! because this crate depends on the radio and so can drive both sides. They
//! meet over a simulated air link that carries the real 51-byte announces and
//! acks and 68-byte beacons on whichever channel each side is tuned to, with
//! loss, delay, reordering and duplication. Everything runs in one thread on
//! tokio's paused clock, so a 16-minute run takes a fraction of a second, and
//! every loss, delay, crash, failed retune, blackout, bind window and operator
//! request is drawn from one seeded generator ([`rng`]): a seed names a run
//! exactly, and the meta-tests check that one seed replays byte for byte.
//!
//! The matrix ([`scenarios`]) proves logic and coverage, not RF timing. The
//! invariants live in [`check`].
//!
//! Every test name starts `dst_`: `cargo test -p ados-groundlink --test
//! hop_dst` runs them. `DST_SEED=<n>` runs seed `n` alone, `DST_SEEDS=<n>`
//! widens a sweep to seeds `1..=n`, `DST_TRACE=1` prints a failing run's whole
//! trace and `DST_DUMP=<dir>` writes every run's trace to
//! `<dir>/<scenario>-<seed>.log`.

mod check;
mod drone;
mod ground;
mod rng;
mod scenarios;
mod sim;
mod world;

use std::time::Instant;

use scenarios::ALL;

/// Seeds run per scenario.
fn seeds() -> Vec<u64> {
    let env = |k| std::env::var(k).ok().and_then(|s| s.parse::<u64>().ok());
    match (env("DST_SEED"), env("DST_SEEDS")) {
        (Some(seed), _) => vec![seed],
        (None, n) => (1..=n.unwrap_or(4)).collect(),
    }
}

/// Run `name` on every matrix seed; fail with a reproducer on any break, or
/// when the seeds together did not exercise what the scenario is for.
fn matrix(name: &str) {
    run_seeds(name, seeds());
}

fn run_seeds(name: &str, list: Vec<u64>) {
    let scenario = ALL
        .iter()
        .find(|s| s.name == name)
        .expect("a scenario of that name");
    let wall = Instant::now();
    let mut sim_s = 0.0;
    let mut total = world::Stats::default();
    for &seed in &list {
        let run = sim::simulate(seed, (scenario.faults)());
        sim_s += run.sim_s;
        add(&mut total, &run.stats);
        if let Some(dir) = std::env::var_os("DST_DUMP") {
            let path = std::path::Path::new(&dir).join(format!("{name}-{seed}.log"));
            std::fs::write(path, &run.trace.bytes).expect("trace dump written");
        }
        if !run.violations.is_empty() {
            let lines = run.trace.lines();
            let keep = if std::env::var_os("DST_TRACE").is_some() {
                lines.len()
            } else {
                80
            };
            let tail = lines[lines.len().saturating_sub(keep)..].join("\n");
            panic!(
                "dst hop scenario `{name}` seed {seed} broke:\n  {}\n\
                 reproduce: DST_SEED={seed} DST_TRACE=1 \
                 cargo test -p ados-groundlink --test hop_dst dst_{name}\n\
                 trace tail:\n{tail}",
                run.violations.join("\n  ")
            );
        }
    }
    let wall_s = wall.elapsed().as_secs_f64();
    eprintln!(
        "dst {name}: {} seeds, {sim_s:.0} sim s in {wall_s:.2} wall s ({:.0}x real time); {total:?}",
        list.len(),
        sim_s / wall_s
    );
    if list.len() > 1 {
        if let Err(what) = (scenario.covers)(&total) {
            panic!("dst hop scenario `{name}`: its seeds never exercised {what}: {total:?}");
        }
    }
}

fn add(t: &mut world::Stats, s: &world::Stats) {
    t.hops_announced += s.hops_announced;
    t.hops_executed += s.hops_executed;
    t.hops_unacked += s.hops_unacked;
    t.lost_acks += s.lost_acks;
    t.acked_unheard += s.acked_unheard;
    t.own_channel_announces += s.own_channel_announces;
    t.drone_retune_fails += s.drone_retune_fails;
    t.follows += s.follows;
    t.rx_restarts += s.rx_restarts;
    t.splits_healed += s.splits_healed;
    t.unheard_checked += s.unheard_checked;
    t.crashes += s.crashes;
    t.bringups += s.bringups;
    t.return_homes += s.return_homes;
    t.outages += s.outages;
    t.binds += s.binds;
    t.manual_accepted += s.manual_accepted;
    t.manual_rejected += s.manual_rejected;
    t.hop_checks += s.hop_checks;
    t.hop_checks_skipped += s.hop_checks_skipped;
    t.rejoin_checks += s.rejoin_checks;
}

#[test]
fn dst_clean() {
    matrix("clean");
}

#[test]
fn dst_lossy_acks() {
    matrix("lossy_acks");
}

#[test]
fn dst_retune_faults() {
    matrix("retune_faults");
}

#[test]
fn dst_crashes() {
    matrix("crashes");
}

#[test]
fn dst_outages() {
    matrix("outages");
}

#[test]
fn dst_two_grounds() {
    matrix("two_grounds");
}

#[test]
fn dst_bind_windows() {
    matrix("bind_windows");
}

#[test]
fn dst_manual() {
    matrix("manual");
}

#[test]
fn dst_periodic() {
    matrix("periodic");
}

#[test]
fn dst_chaos() {
    matrix("chaos");
}

/// Seeds the wide sweeps found: a bring-up on home that left the drone's hop
/// record on the old hop channel, which a failed hop's call-back then named.
/// (A ground follow reverting to the presence cache's hint instead of the
/// channel it left breaks `bind_windows` seed 2 and `chaos` seed 4 of the
/// matrix itself.)
#[test]
fn dst_regression_seeds() {
    run_seeds("retune_faults", vec![222]);
    run_seeds("chaos", vec![58]);
}

/// One seed replays byte for byte.
#[test]
fn dst_same_seed_replays_byte_identical() {
    for s in ALL {
        let a = sim::simulate(7, (s.faults)()).trace;
        let b = sim::simulate(7, (s.faults)()).trace;
        if let Some((line, x, y)) = a.first_difference(&b) {
            panic!(
                "{}: seed 7 diverged at trace line {line}:\n{x}\n{y}",
                s.name
            );
        }
        assert_eq!(a.bytes, b.bytes, "{}", s.name);
        assert!(a.bytes.len() > 1000, "{}: a trace was written", s.name);
    }
}

/// Different seeds run different hops.
#[test]
fn dst_different_seeds_differ() {
    for s in ALL {
        let a = sim::simulate(11, (s.faults)()).trace;
        let b = sim::simulate(12, (s.faults)()).trace;
        assert_ne!(
            a.bytes, b.bytes,
            "{}: seeds 11 and 12 gave one trace",
            s.name
        );
    }
}
