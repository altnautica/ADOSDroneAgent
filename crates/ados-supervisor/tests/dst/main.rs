//! In-process deterministic simulation (DST) of the over-the-air WFB bind.
//!
//! The real bind state machine ([`ados_supervisor::bind::orchestrator::BindOrchestrator`])
//! runs on both rigs at once, a ground station and a freshly flashed drone,
//! each over a simulated host ([`host`]): its service units, key and config
//! files, bind tunnel and supervisor process. The two meet over a simulated
//! air link ([`wire`]) that carries the upstream bind conversation segment by
//! segment. Everything runs in one thread on tokio's paused clock, so the bind
//! watchdog, the transfer and restart budgets and every retry run in simulated
//! seconds, and every latency, loss, outage, kill and cancel is drawn from one
//! seeded generator ([`rng`]): a seed names a run exactly, and the meta-tests
//! check that one seed replays byte for byte.
//!
//! The matrix ([`scenarios`]) proves logic and coverage, not timing on a rig:
//! a clean bind, a lossy link with outages and failing units, a drone with no
//! peer, a paired ground station binding while its drone never opens a
//! window, a supervisor killed mid-bind, an operator cancel, a second bind
//! racing the first, a slow service manager and a drone paired with no fleet
//! slot issued ([`unissued`]). The invariants live in [`check`].
//!
//! Every test name starts `dst_`: `cargo test -p ados-supervisor --test dst`
//! runs them.

mod check;
mod host;
mod reconcile;
mod rng;
mod scenarios;
mod sim;
mod unissued;
mod wire;
mod world;

use std::time::Instant;

use scenarios::ALL;

/// Seeds run per scenario in the matrix. `DST_SEED=<n>` runs seed `n` alone
/// (a reproducer), `DST_SEEDS=<n>` widens a sweep to seeds `1..=n`, and
/// `DST_TRACE=1` prints a failing run's whole trace.
fn seeds() -> std::ops::RangeInclusive<u64> {
    let env = |k| std::env::var(k).ok().and_then(|s| s.parse::<u64>().ok());
    match (env("DST_SEED"), env("DST_SEEDS")) {
        (Some(seed), _) => seed..=seed,
        (None, n) => 1..=n.unwrap_or(8),
    }
}

/// Run `name` on every matrix seed; fail with a reproducer on any break.
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
    let mut sim_s = 0.0;
    let mut tally = [0u32; 3];
    let mut reconciled = 0u32;
    for &seed in &list {
        let (run, allowed) = scenario(seed);
        sim_s += run.sim_s;
        tally[run.outcome as usize] += 1;
        if run.before_reconcile == check::Outcome::Split && run.outcome == check::Outcome::Bound {
            reconciled += 1;
        }
        if std::env::var_os("DST_TRACE").is_some() {
            eprintln!(
                "dst {name} seed {seed}: {:?} (operators left {:?}, kills {:?})",
                run.outcome, run.before_reconcile, run.kills
            );
            if std::env::var_os("DST_SEED").is_some() {
                eprintln!("{}", run.trace.lines().join("\n"));
            }
        }
        let mut breaks = run.violations.clone();
        if !allowed.contains(&run.outcome) {
            breaks.push(format!(
                "outcome {:?}, expected one of {allowed:?}",
                run.outcome
            ));
        }
        if !breaks.is_empty() {
            let lines = run.trace.lines();
            let keep = if std::env::var_os("DST_TRACE").is_some() {
                lines.len()
            } else {
                60
            };
            let tail = lines[lines.len().saturating_sub(keep)..].join("\n");
            panic!(
                "dst bind scenario `{name}` seed {seed} broke:\n  {}\n\
                 reproduce: DST_SEED={seed} DST_TRACE=1 \
                 cargo test -p ados-supervisor --test dst dst_{name}\n\
                 trace tail:\n{tail}",
                breaks.join("\n  ")
            );
        }
    }
    let wall_s = wall.elapsed().as_secs_f64();
    eprintln!(
        "dst {name}: {} seeds ({} bound, {} aborted, {} split; {reconciled} split by a fault in \
         the last exchange and reconciled by auto-pair), {sim_s:.0} sim s in {wall_s:.2} wall s \
         ({:.0}x real time)",
        list.len(),
        tally[0],
        tally[1],
        tally[2],
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
fn dst_no_peer() {
    matrix("no_peer");
}

#[test]
fn dst_restart() {
    matrix("restart");
}

/// Seeds the wide sweeps found: supervisors killed past their peer-evidence
/// gate (the restart must finish the apply, no split), and splits a fault in
/// the last exchange left that auto-pair must reconcile.
#[test]
fn dst_regression_seeds() {
    run_seeds("restart", vec![11, 170, 268, 778, 818]);
    run_seeds("lossy", vec![290, 323]);
    run_seeds("cancel", vec![778, 818]);
}

#[test]
fn dst_cancel() {
    matrix("cancel");
}

#[test]
fn dst_concurrent() {
    matrix("concurrent");
}

#[test]
fn dst_slow_systemd() {
    matrix("slow_systemd");
}

#[test]
fn dst_ground_misses_ok() {
    matrix("ground_misses_ok");
}

#[test]
fn dst_ground_rebinds_alone() {
    matrix("ground_rebinds_alone");
}

#[test]
fn dst_unissued_drone() {
    matrix("unissued_drone");
}

/// One seed replays byte for byte.
#[test]
fn dst_same_seed_replays_byte_identical() {
    for (name, scenario) in ALL {
        let a = scenario(7).0.trace;
        let b = scenario(7).0.trace;
        if let Some((line, x, y)) = a.first_difference(&b) {
            panic!("{name}: seed 7 diverged at trace line {line}:\n{x}\n{y}");
        }
        assert_eq!(a.bytes, b.bytes, "{name}");
        assert!(a.bytes.len() > 300, "{name}: a trace was written");
    }
}

/// Different seeds run different binds.
#[test]
fn dst_different_seeds_differ() {
    for (name, scenario) in ALL {
        let a = scenario(11).0.trace;
        let b = scenario(12).0.trace;
        assert_ne!(a.bytes, b.bytes, "{name}: seeds 11 and 12 gave one trace");
    }
}
