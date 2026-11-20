//! In-process deterministic simulation (DST) of the cloud command path.
//!
//! The real command-poll pass (`ados_cloud::loops::command_poll::poll_once`)
//! and the real executed-command record (on a temp dir) run against a
//! simulated cloud queue ([`queue`]: the backend's claim and ack rules, lease,
//! attempt budget and delivery window) over a simulated network that delays,
//! loses, duplicates and reorders, with outages; behind the runner seam a
//! simulated local API acts and answers ([`node`]). The node is killed mid-pass
//! and restarted, and a restart of its own unit kills it mid-command. A
//! heartbeat loop shares the network and folds its outcomes into the real
//! cloud-link tracker. Everything runs in one thread on tokio's paused clock,
//! with no socket, and every latency, loss and fault is drawn from one seeded
//! generator ([`rng`]): a seed names a run exactly, and the meta-tests check
//! that one seed replays byte for byte.
//!
//! The invariants live in [`check`]. Every test name starts `dst_`:
//! `cargo test -p ados-cloud --test dst` runs them.

mod check;
mod node;
mod queue;
mod rng;
mod scenarios;
mod sim;
mod world;

use std::time::Instant;

use scenarios::ALL;

/// Seeds run per scenario. `DST_SEED=<n>` runs seed `n` alone (a reproducer),
/// `DST_SEEDS=<n>` widens a sweep to seeds `1..=n`, and `DST_TRACE=1` prints a
/// failing run's whole trace.
fn seeds() -> std::ops::RangeInclusive<u64> {
    let env = |k| std::env::var(k).ok().and_then(|s| s.parse::<u64>().ok());
    match (env("DST_SEED"), env("DST_SEEDS")) {
        (Some(seed), _) => seed..=seed,
        (None, n) => 1..=n.unwrap_or(6),
    }
}

fn matrix(name: &str) {
    run_seeds(name, seeds().collect());
}

/// Run `name` on `list`; fail with a reproducer on any break.
fn run_seeds(name: &str, list: Vec<u64>) {
    let (_, scenario) = ALL
        .iter()
        .find(|(n, _)| *n == name)
        .expect("a scenario of that name");
    let wall = Instant::now();
    let (mut sim_s, mut commands, mut ran, mut refused, mut kills) = (0.0, 0, 0, 0, 0);
    for &seed in &list {
        let run = scenario(seed);
        sim_s += run.sim_s;
        commands += run.commands;
        ran += run.ran;
        refused += run.refused;
        kills += run.kills;
        if !run.violations.is_empty() {
            let lines = run.trace.lines();
            let keep = if std::env::var_os("DST_TRACE").is_some() {
                lines.len()
            } else {
                80
            };
            let tail = lines[lines.len().saturating_sub(keep)..].join("\n");
            panic!(
                "dst cloud-command scenario `{name}` seed {seed} broke:\n  {}\n\
                 reproduce: DST_SEED={seed} DST_TRACE=1 \
                 cargo test -p ados-cloud --test dst dst_{name}\n\
                 trace tail:\n{tail}",
                run.violations.join("\n  ")
            );
        }
    }
    let wall_s = wall.elapsed().as_secs_f64();
    eprintln!(
        "dst {name}: {} seeds, {commands} commands ({ran} ran, {refused} refused unrun), \
         {kills} kills, {sim_s:.0} sim s in {wall_s:.2} wall s ({:.0}x real time)",
        list.len(),
        sim_s / wall_s
    );
}

#[test]
fn dst_clean() {
    matrix("clean");
}

#[test]
fn dst_lossy() {
    matrix("lossy");
}

#[test]
fn dst_outage() {
    matrix("outage");
}

#[test]
fn dst_restart() {
    matrix("restart");
}

#[test]
fn dst_slow_batch() {
    matrix("slow_batch");
}

#[test]
fn dst_storm() {
    matrix("storm");
}

/// The seeds that found the bugs this simulation fixed, pinned whatever
/// `DST_SEEDS` says: a restart of the relay's own unit ran again on its
/// redelivery (restart 1, storm 1); flight commands queued behind slower ones
/// in one claim ran after their delivery window (clean 1, slow_batch 1); and an
/// ack lost to an outage was overtaken by a claim that failed its row as never
/// acknowledged (outage 6).
#[test]
fn dst_regression_seeds() {
    run_seeds("restart", vec![1]);
    run_seeds("storm", vec![1]);
    run_seeds("clean", vec![1]);
    run_seeds("slow_batch", vec![1]);
    run_seeds("outage", vec![6]);
}

/// One seed replays byte for byte.
#[test]
fn dst_same_seed_replays_byte_identical() {
    for (name, scenario) in ALL {
        let a = scenario(7).trace;
        let b = scenario(7).trace;
        if let Some((line, x, y)) = a.first_difference(&b) {
            panic!("{name}: seed 7 diverged at trace line {line}:\n{x}\n{y}");
        }
        assert_eq!(a.bytes, b.bytes, "{name}");
        assert!(a.bytes.len() > 300, "{name}: a trace was written");
    }
}

/// Different seeds run different traces.
#[test]
fn dst_different_seeds_differ() {
    for (name, scenario) in ALL {
        let a = scenario(11).trace;
        let b = scenario(12).trace;
        assert_ne!(a.bytes, b.bytes, "{name}: seeds 11 and 12 gave one trace");
    }
}
